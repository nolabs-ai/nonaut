use std::{
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use diffy::create_patch;
use directories::BaseDirs;

use crate::policy::OperationAccess;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

const MAX_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DIFF_LINES: usize = 80;
const MAX_DIFF_BYTES: usize = 16 * 1024;

mod catalog;
mod sandbox;

pub(crate) use catalog::specs;

pub async fn execute(workspace: &Path, tool: &str, arguments: &Value) -> Result<String> {
    let arguments = normalize_tool_paths(workspace, tool, arguments)?;
    let (operation, fallback_root, access) = operation_location(workspace, tool, &arguments)?;
    let policy = crate::policy::load(workspace, &operation, &fallback_root, access)?;
    let operation_workspace = policy.workspace.clone();
    if tool == "run_command" {
        return run_command(&operation_workspace, &arguments, &policy).await;
    }

    let executable = std::env::current_exe().context("could not locate the Nonaut executable")?;
    let mut child = sandbox::process(&operation_workspace, &executable, &policy)?;
    child
        .arg("tool")
        .arg(&operation_workspace)
        .arg(tool)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = child
        .spawn()
        .context("could not start the sandboxed tool")?;
    let mut group = ProcessGroup::new(child.id());
    let encoded = serde_json::to_vec(&arguments).context("could not encode tool arguments")?;
    let mut stdin = child
        .stdin
        .take()
        .context("tool worker stdin was unavailable")?;
    stdin
        .write_all(&encoded)
        .await
        .context("could not send arguments to the tool worker")?;
    drop(stdin);
    let limit = policy.tool_timeout_seconds;
    let output = tokio::time::timeout(Duration::from_secs(limit), child.wait_with_output())
        .await
        .with_context(|| format!("tool timed out after {limit} seconds"))?
        .context("could not wait for the sandboxed tool")?;
    group.finished();
    output_result(tool, output)
}

pub(crate) async fn execute_shell(
    session_root: &Path,
    cwd: &Path,
    command: String,
) -> Result<String> {
    execute(
        session_root,
        "run_command",
        &json!({
            "program": "sh",
            "args": ["-c", command],
            "cwd": cwd,
        }),
    )
    .await
}

fn operation_location(
    session_root: &Path,
    tool: &str,
    arguments: &Value,
) -> Result<(PathBuf, PathBuf, OperationAccess)> {
    if tool == "run_command" {
        let cwd = PathBuf::from(required_string(arguments, "cwd")?);
        return Ok((cwd.clone(), cwd, OperationAccess::Write));
    }
    let path = arguments
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| session_root.to_path_buf());
    let fallback = if path.is_dir() {
        path.clone()
    } else {
        path.parent().unwrap_or(session_root).to_path_buf()
    };
    let access = if matches!(tool, "read_file" | "list_directory" | "search_files") {
        OperationAccess::Read
    } else {
        OperationAccess::Write
    };
    Ok((path, fallback, access))
}

fn normalize_tool_paths(workspace: &Path, tool: &str, arguments: &Value) -> Result<Value> {
    let mut expanded = arguments.clone();
    let home = BaseDirs::new()
        .context("could not determine the user's home directory")?
        .home_dir()
        .to_path_buf();

    if matches!(
        tool,
        "read_file"
            | "list_directory"
            | "write_file"
            | "search_files"
            | "edit_file"
            | "delete_file"
    ) && let Some(path) = expanded.get_mut("path")
        && let Some(raw) = path.as_str()
    {
        let expanded_path = expand_tilde_with_home(raw, &home);
        *path = Value::String(
            normalize_path(workspace, &expanded_path)?
                .to_string_lossy()
                .into(),
        );
    }

    if tool == "run_command"
        && let Some(args) = expanded.get_mut("args").and_then(Value::as_array_mut)
    {
        for argument in args {
            if let Some(raw) = argument.as_str() {
                *argument = Value::String(expand_tildes_in_command_argument(raw, &home));
            }
        }
    }
    if tool == "run_command"
        && let Some(cwd) = expanded.get_mut("cwd")
        && let Some(raw) = cwd.as_str()
    {
        let expanded_path = expand_tilde_with_home(raw, &home);
        *cwd = Value::String(
            normalize_path(workspace, &expanded_path)?
                .to_string_lossy()
                .into(),
        );
    }

    Ok(expanded)
}

fn normalize_path(workspace: &Path, raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if path.as_os_str().is_empty() {
        bail!("path may not be empty");
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    Ok(normalized)
}

fn expand_tilde_with_home(raw: &str, home: &Path) -> String {
    let normalized = raw.trim_start_matches("./");
    if normalized == "~" {
        return home.to_string_lossy().into_owned();
    }
    if let Some(suffix) = normalized.strip_prefix("~/") {
        return home.join(suffix).to_string_lossy().into_owned();
    }
    raw.to_owned()
}

fn expand_tildes_in_command_argument(raw: &str, home: &Path) -> String {
    let mut output = String::with_capacity(raw.len());
    let mut escaped = false;
    let mut previous = None;
    let home = home.to_string_lossy();
    let mut characters = raw.char_indices().peekable();

    while let Some((index, character)) = characters.next() {
        if escaped {
            output.push(character);
            escaped = false;
            previous = Some(character);
            continue;
        }
        match character {
            '\\' => {
                output.push(character);
                escaped = true;
            }
            '~' if is_shell_word_start(previous)
                && matches!(characters.peek(), None | Some((_, '/'))) =>
            {
                output.push_str(&home);
            }
            _ => output.push(character),
        }
        previous = raw[index..].chars().next();
    }
    output
}

fn is_shell_word_start(previous: Option<char>) -> bool {
    previous.is_none_or(|character| {
        character.is_ascii_whitespace()
            || matches!(
                character,
                '=' | ':' | '>' | '<' | '(' | '{' | ';' | '|' | '&' | '\'' | '"'
            )
    })
}

async fn run_command(
    workspace: &Path,
    arguments: &Value,
    policy: &crate::policy::Policy,
) -> Result<String> {
    let program = required_string(arguments, "program")?;
    if program.trim().is_empty() {
        bail!("program may not be empty");
    }
    let args = arguments["args"]
        .as_array()
        .context("args must be an array")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("every command argument must be a string")
        })
        .collect::<Result<Vec<_>>>()?;
    let command_cwd = optional_string(arguments, "cwd")
        .map(|cwd| cwd.unwrap_or_else(|| workspace.to_str().unwrap_or(".")))?;
    // The model may ask for less time than the policy allows, never more.
    let limit = policy.tool_timeout_seconds;
    let timeout = arguments["timeout_seconds"]
        .as_u64()
        .unwrap_or(limit)
        .clamp(1, limit);

    let executable = std::env::current_exe().context("could not locate the Nonaut executable")?;
    let mut command = sandbox::process(workspace, &executable, policy)?;
    command
        .arg("command")
        .arg(workspace)
        .arg(command_cwd)
        .arg(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = command
        .spawn()
        .with_context(|| format!("could not start sandboxed command `{program}`"))?;
    let mut group = ProcessGroup::new(child.id());
    let output = tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output())
        .await
        .with_context(|| format!("command timed out after {timeout} seconds"))?
        .context("could not wait for sandboxed command")?;
    group.finished();
    output_result(program, output)
}

/// Kills a sandboxed tool's whole process group if the call is abandoned by
/// a timeout or an interrupt. `kill_on_drop` alone only reaches the direct
/// child, leaving anything it spawned (a build, `sleep`) running.
struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    fn new(pid: Option<u32>) -> Self {
        Self(pid)
    }

    /// The tool exited normally; leave its group alone.
    fn finished(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|pid| i32::try_from(pid).ok()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

fn output_result(name: &str, output: std::process::Output) -> Result<String> {
    let mut combined = String::new();
    if !output.stdout.is_empty() {
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    let combined = truncate(&combined);
    if output.status.success() {
        if combined.trim().is_empty() {
            Ok(format!("{name} completed successfully"))
        } else {
            Ok(combined)
        }
    } else {
        if is_sandbox_denial(&combined) {
            // nono denials surface as ordinary EACCES/EPERM errors, so ordinary
            // permission failures look identical. Keep the output either way.
            bail!(
                "Likely blocked by the nono sandbox: {name} hit a permission error.\n{}",
                combined.trim_end()
            );
        }
        Err(anyhow!(
            "{name} exited with {}\n{}",
            output.status,
            combined.trim_end()
        ))
    }
}

fn is_sandbox_denial(output: &str) -> bool {
    [
        "Operation not permitted",
        "Permission denied",
        "sandbox-denied",
        "PolicyBlocked",
        "sandbox_denied_path",
        "CWD access requires --allow-cwd",
    ]
    .iter()
    .any(|marker| output.contains(marker))
}

fn truncate(value: &str) -> String {
    if value.len() <= MAX_OUTPUT_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated by Nonaut]", &value[..end])
}

pub fn run_sandbox() -> Result<()> {
    sandbox::run()
}

fn run_tool_worker(workspace: &Path, tool: &str) -> Result<()> {
    let mut encoded = String::new();
    io::stdin()
        .take((MAX_OUTPUT_BYTES * 4) as u64)
        .read_to_string(&mut encoded)
        .context("could not read tool arguments")?;
    let arguments: Value = serde_json::from_str(&encoded).context("invalid tool arguments")?;
    let output = match tool {
        "read_file" => read_file(workspace, &arguments),
        "list_directory" => list_directory(workspace, &arguments),
        "write_file" => write_file(workspace, &arguments),
        "search_files" => search_files(workspace, &arguments),
        "edit_file" => edit_file(workspace, &arguments),
        "delete_file" => delete_file(workspace, &arguments),
        _ => bail!("unknown tool `{tool}`"),
    }?;
    print!("{output}");
    Ok(())
}

fn read_file(workspace: &Path, arguments: &Value) -> Result<String> {
    let path = requested_path(workspace, required_string(arguments, "path")?)?;
    let metadata = fs::metadata(&path).context("could not inspect file")?;
    if !metadata.is_file() {
        bail!("path is not a regular file");
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!("file exceeds the 2 MiB read limit");
    }
    let content = fs::read_to_string(&path).context("file is not valid UTF-8 text")?;
    let start = arguments["start_line"].as_u64().unwrap_or(1).max(1) as usize;
    let end = arguments["end_line"].as_u64().unwrap_or(u64::MAX) as usize;
    if end < start {
        bail!("end_line must be greater than or equal to start_line");
    }
    Ok(truncate(&select_lines(&content, start, end)))
}

fn select_lines(content: &str, start: usize, end: usize) -> String {
    let mut output = String::new();
    for (index, line) in content.lines().enumerate() {
        let number = index + 1;
        if number >= start && number <= end {
            // Keep presentation metadata out of file content. Numbered lines
            // are easy for a model to accidentally copy into edit_file.
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

fn list_directory(workspace: &Path, arguments: &Value) -> Result<String> {
    let root = requested_path(workspace, required_string(arguments, "path")?)?;
    let metadata = fs::metadata(&root).context("could not inspect directory")?;
    if !metadata.is_dir() {
        bail!("path is not a directory");
    }
    let depth = arguments["depth"].as_u64().unwrap_or(2).clamp(1, 8) as usize;
    let mut entries = Vec::new();
    visit_directory(workspace, &root, depth, &mut entries)?;
    entries.sort();
    if entries.is_empty() {
        Ok("[empty directory]".to_owned())
    } else {
        Ok(truncate(&entries.join("\n")))
    }
}

fn visit_directory(
    workspace: &Path,
    directory: &Path,
    depth: usize,
    output: &mut Vec<String>,
) -> Result<()> {
    if output.len() >= 2_000 {
        return Ok(());
    }
    let mut entries = fs::read_dir(directory)
        .context("could not read directory")?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(workspace).unwrap_or(&path);
        let kind = entry.file_type()?;
        output.push(format!(
            "{}{}",
            relative.display(),
            if kind.is_dir() { "/" } else { "" }
        ));
        let name = entry.file_name();
        if depth > 1 && kind.is_dir() && name != ".git" && name != "target" && !kind.is_symlink() {
            visit_directory(workspace, &path, depth - 1, output)?;
        }
    }
    Ok(())
}

fn search_files(workspace: &Path, arguments: &Value) -> Result<String> {
    let query = required_string(arguments, "query")?;
    let path = requested_path(
        workspace,
        optional_string(arguments, "path")?.unwrap_or("."),
    )?;
    let mut command = std::process::Command::new("rg");
    command.args([
        "--line-number",
        "--column",
        "--no-heading",
        "--color",
        "never",
    ]);
    if let Some(glob) = optional_string(arguments, "glob")? {
        command.args(["--glob", glob]);
    }
    let output = command
        .arg(query)
        .arg(path)
        .output()
        .context("could not run ripgrep; install `rg` to use search_files")?;
    if output.status.code() == Some(1) {
        return Ok("No matches found.".to_owned());
    }
    output_result("rg", output)
}

fn write_file(workspace: &Path, arguments: &Value) -> Result<String> {
    let raw_path = required_string(arguments, "path")?;
    let path = requested_path(workspace, raw_path)?;
    let shown_path = display_path(workspace, &path);
    let content = required_string(arguments, "content")?;
    match fs::metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                bail!("path is not a regular file");
            }
            let previous = fs::read_to_string(&path).context("file is not valid UTF-8 text")?;
            fs::write(&path, content).context("could not replace file")?;
            Ok(write_result("Updated", &shown_path, &previous, content))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().context("file path has no parent")?;
            let metadata = fs::metadata(parent).context("could not inspect parent directory")?;
            if !metadata.is_dir() {
                bail!("parent path is not a directory");
            }
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            use std::io::Write;
            options
                .open(&path)
                .context("could not create file")?
                .write_all(content.as_bytes())
                .context("could not write new file")?;
            Ok(write_result("Created", &shown_path, "", content))
        }
        Err(error) => Err(error).context("could not inspect file"),
    }
}

fn edit_file(workspace: &Path, arguments: &Value) -> Result<String> {
    let raw_path = required_string(arguments, "path")?;
    let path = requested_path(workspace, raw_path)?;
    let shown_path = display_path(workspace, &path);
    let current = fs::read_to_string(&path).context("could not read file to edit")?;
    let requested_old_text = required_string(arguments, "old_text")?;
    let new_text = required_string(arguments, "new_text")?;
    if requested_old_text.is_empty() {
        bail!("old_text may not be empty");
    }
    let numbered_text = strip_read_line_numbers(requested_old_text);
    let old_text = if current.contains(requested_old_text) {
        requested_old_text
    } else if let Some(numbered_text) = numbered_text.as_deref()
        && current.contains(numbered_text)
    {
        numbered_text
    } else {
        bail!("old_text was not found in the file");
    };
    let occurrences = current.matches(old_text).count();
    let replace_all = arguments["replace_all"].as_bool().unwrap_or(false);
    if occurrences > 1 && !replace_all {
        bail!("old_text occurs {occurrences} times; provide more context or set replace_all");
    }
    let updated = if replace_all {
        current.replace(old_text, new_text)
    } else {
        current.replacen(old_text, new_text, 1)
    };
    fs::write(&path, &updated).context("could not write edited file")?;
    Ok(write_result("Updated", &shown_path, &current, &updated))
}

fn strip_read_line_numbers(text: &str) -> Option<String> {
    let mut output = Vec::new();
    for line in text.lines() {
        let (number, content) = line.split_once(": ")?;
        number.parse::<usize>().ok()?;
        output.push(content);
    }
    (!output.is_empty()).then(|| output.join("\n"))
}

fn delete_file(workspace: &Path, arguments: &Value) -> Result<String> {
    let raw_path = required_string(arguments, "path")?;
    let path = requested_path(workspace, raw_path)?;
    let shown_path = display_path(workspace, &path);
    let metadata = fs::metadata(&path).context("could not inspect file to delete")?;
    if !metadata.is_file() {
        bail!("delete only supports regular files");
    }
    let previous = fs::read_to_string(&path).ok();
    fs::remove_file(&path).context("could not delete file")?;
    Ok(previous.map_or_else(
        || format!("Deleted {shown_path} (diff unavailable for non-text content)"),
        |content| write_result("Deleted", &shown_path, &content, ""),
    ))
}

fn display_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn write_result(action: &str, path: &str, before: &str, after: &str) -> String {
    let patch = create_patch(before, after).to_string();
    let body = patch
        .strip_prefix("--- original\n+++ modified\n")
        .unwrap_or(&patch);
    let headers = match action {
        "Created" => format!("--- /dev/null\n+++ b/{path}\n"),
        "Deleted" => format!("--- a/{path}\n+++ /dev/null\n"),
        _ => format!("--- a/{path}\n+++ b/{path}\n"),
    };
    let diff = truncate_diff(&format!("{headers}{body}"));
    format!("{action} {path}\n{diff}")
}

fn truncate_diff(diff: &str) -> String {
    let mut output = String::new();
    let mut truncated = false;
    for (index, line) in diff.lines().enumerate() {
        let additional = line.len().saturating_add(1);
        if index >= MAX_DIFF_LINES || output.len().saturating_add(additional) > MAX_DIFF_BYTES {
            truncated = true;
            break;
        }
        output.push_str(line);
        output.push('\n');
    }
    if truncated {
        output.push_str("… diff truncated by Nonaut\n");
    }
    output.trim_end().to_owned()
}

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str> {
    arguments[name]
        .as_str()
        .with_context(|| format!("{name} must be a string"))
}

fn optional_string<'a>(arguments: &'a Value, name: &str) -> Result<Option<&'a str>> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .with_context(|| format!("{name} must be a string")),
    }
}

fn requested_path(workspace: &Path, raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if path.as_os_str().is_empty() {
        bail!("path may not be empty");
    }
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(workspace.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_paths_are_left_for_nono_to_enforce() {
        let workspace = Path::new("/workspace");
        assert_eq!(
            requested_path(workspace, "/outside/secret").unwrap(),
            PathBuf::from("/outside/secret")
        );
        assert_eq!(
            requested_path(workspace, "src/main.rs").unwrap(),
            PathBuf::from("/workspace/src/main.rs")
        );
    }

    #[test]
    fn structured_tool_paths_are_absolute_and_lexically_normalized() {
        let workspace = Path::new("/Users/example/Work/nonaut");
        let arguments = json!({ "path": "./src/../test.md" });
        let normalized = normalize_tool_paths(workspace, "read_file", &arguments).unwrap();
        assert_eq!(
            normalized["path"],
            Value::String("/Users/example/Work/nonaut/test.md".to_owned())
        );
    }

    #[test]
    fn normalization_does_not_restrict_paths_before_nono() {
        let workspace = Path::new("/Users/example/Work/nonaut");
        assert_eq!(
            normalize_path(workspace, "../../.ssh/config").unwrap(),
            PathBuf::from("/Users/example/.ssh/config")
        );
    }

    #[test]
    fn command_cwd_is_normalized_without_policy_enforcement() {
        let workspace = Path::new("/Users/example/Work/nonaut");
        let arguments = json!({
            "program": "ls",
            "args": ["."],
            "cwd": "/Users/example/.ssh"
        });
        let normalized = normalize_tool_paths(workspace, "run_command", &arguments).unwrap();
        assert_eq!(normalized["cwd"], "/Users/example/.ssh");
    }

    #[test]
    fn expands_current_user_tilde_before_sandboxing() {
        let home = Path::new("/Users/example");
        assert_eq!(
            expand_tilde_with_home("~/.ssh", home),
            "/Users/example/.ssh"
        );
        assert_eq!(expand_tilde_with_home("~", home), "/Users/example");
        assert_eq!(
            expand_tilde_with_home("./~/Work/test.md", home),
            "/Users/example/Work/test.md"
        );
        assert_eq!(expand_tilde_with_home("~other/file", home), "~other/file");
    }

    #[test]
    fn expands_tildes_at_shell_word_boundaries() {
        let home = Path::new("/Users/example");
        assert_eq!(
            expand_tildes_in_command_argument("cat > ~/Work/test.md", home),
            "cat > /Users/example/Work/test.md"
        );
        assert_eq!(
            expand_tildes_in_command_argument("ls ~/.ssh && echo done", home),
            "ls /Users/example/.ssh && echo done"
        );
    }

    #[test]
    fn expands_quoted_shell_tildes_but_preserves_escaped_literals() {
        let home = Path::new("/Users/example");
        assert_eq!(
            expand_tildes_in_command_argument(
                "echo '~/literal' \"~/also-literal\" \\~/escaped",
                home
            ),
            "echo '/Users/example/literal' \"/Users/example/also-literal\" \\~/escaped"
        );
    }

    #[test]
    fn recognizes_sandbox_denial_messages() {
        assert!(is_sandbox_denial("ls: secret: Operation not permitted"));
        assert!(is_sandbox_denial("reason: PolicyBlocked"));
        assert!(is_sandbox_denial(
            "nono: CWD access requires --allow-cwd in non-interactive mode"
        ));
        assert!(!is_sandbox_denial("cargo test exited with status 1"));
    }

    #[test]
    fn write_results_include_a_compact_named_diff() {
        let result = write_result("Created", "test.md", "", "some random words\n");
        assert!(result.starts_with("Created test.md\n--- /dev/null\n+++ b/test.md\n"));
        assert!(result.contains("+some random words"));
    }

    #[test]
    fn workspace_paths_are_shortened_for_display() {
        let workspace = Path::new("/Users/example/Work/nonaut");
        assert_eq!(
            display_path(
                workspace,
                Path::new("/Users/example/Work/nonaut/src/main.rs")
            ),
            "src/main.rs"
        );
        assert_eq!(
            display_path(workspace, Path::new("/Users/example/Work/test.md")),
            "/Users/example/Work/test.md"
        );
    }

    #[test]
    fn strips_read_file_line_numbers_for_exact_edits() {
        assert_eq!(
            strip_read_line_numbers("1: alpha\n2: beta\n3: gamma\n").as_deref(),
            Some("alpha\nbeta\ngamma")
        );
        assert_eq!(strip_read_line_numbers("alpha\nbeta"), None);
    }

    #[test]
    fn read_ranges_do_not_decorate_file_content() {
        assert_eq!(select_lines("alpha\nbeta\ngamma\n", 2, 3), "beta\ngamma\n");
    }

    #[test]
    fn tool_catalog_contains_only_known_tools() {
        let tools = specs().as_array().unwrap().clone();
        let names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "read_file",
                "list_directory",
                "write_file",
                "search_files",
                "edit_file",
                "delete_file",
                "run_command"
            ]
        );
    }
}
