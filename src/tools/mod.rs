use std::{
    error::Error,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use diffy::create_patch;
use directories::BaseDirs;
use sha2::{Digest, Sha256};

use crate::policy::OperationAccess;
use serde_json::{Value, json};

const MAX_OUTPUT_BYTES: usize = 48 * 1024;
const MAX_OUTPUT_LINES: usize = 1_000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DIFF_LINES: usize = 80;
const MAX_DIFF_BYTES: usize = 16 * 1024;

mod catalog;
mod index;
mod sandbox;

pub(crate) use catalog::specs;

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub(crate) struct ToolContext {
    pub call_id: String,
    pub tool: String,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv_display: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

impl ToolContext {
    pub(crate) fn minimal(call_id: &str, tool: &str) -> Self {
        Self {
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            operation: operation_name(tool).to_owned(),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandProgress {
    pub latest_line: String,
    pub line_count: usize,
}

pub(crate) type ProgressReporter = Arc<dyn Fn(CommandProgress) + Send + Sync>;

#[derive(Clone, Debug)]
#[cfg_attr(
    not(any(feature = "provider-codex", feature = "control-plane", test)),
    allow(dead_code)
)]
pub(crate) struct ToolFailure {
    pub reason: &'static str,
    pub exit_code: Option<i32>,
}

#[derive(Debug)]
struct ProcessFailure {
    name: String,
    status: String,
    exit_code: Option<i32>,
    output: String,
    denied: bool,
}

impl fmt::Display for ProcessFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.denied {
            write!(
                formatter,
                "Likely blocked by the nono sandbox: {} hit a permission error.\n{}",
                self.name,
                self.output.trim_end()
            )
        } else {
            write!(
                formatter,
                "{} exited with {}\n{}",
                self.name,
                self.status,
                self.output.trim_end()
            )
        }
    }
}

impl Error for ProcessFailure {}

pub(crate) fn audit_context(
    call_id: impl Into<String>,
    workspace: &Path,
    tool: &str,
    arguments: &Value,
) -> ToolContext {
    let call_id = call_id.into();
    let mut context = ToolContext::minimal(&call_id, tool);
    if tool == "batch" {
        let Ok(policy) = crate::policy::pin(workspace) else {
            return context;
        };
        let audit = policy.audit_context();
        context.workspace_hash = Some(workspace_hash(&audit.workspace));
        context.profiles = audit.profiles;
        context.policy_digest = Some(audit.policy_digest);
        context.network_mode = Some(audit.network_mode);
        context.target = arguments["calls"]
            .as_array()
            .map(|calls| format!("{} read calls", calls.len()));
        return context;
    }
    let Ok(arguments) = normalize_tool_paths(workspace, tool, arguments) else {
        return context;
    };
    let Ok((operation, fallback_root, access)) = operation_location(workspace, tool, &arguments)
    else {
        return context;
    };
    let Ok(policy) = crate::policy::pin(workspace) else {
        return context;
    };
    if policy
        .authorize(&operation, &fallback_root, access)
        .is_err()
    {
        return context;
    }
    let audit = policy.audit_context();
    context.operation = operation_name(tool).to_owned();
    context.workspace_hash = Some(workspace_hash(&audit.workspace));
    context.profiles = audit.profiles;
    context.policy_digest = Some(audit.policy_digest);
    context.network_mode = Some(audit.network_mode);
    if tool == "run_command" {
        command_context(&mut context, &arguments, &audit.workspace);
        context.timeout_seconds = Some(
            arguments["timeout_seconds"]
                .as_u64()
                .unwrap_or(policy.tool_timeout_seconds)
                .clamp(1, policy.tool_timeout_seconds),
        );
    } else if let Some(path) = arguments.get("path").and_then(Value::as_str) {
        context.target = Some(display_audit_path(&audit.workspace, Path::new(path)));
    }
    context
}

pub(crate) fn audit_failure(error: &anyhow::Error) -> ToolFailure {
    if let Some(failure) = error.downcast_ref::<ProcessFailure>() {
        return ToolFailure {
            reason: if failure.denied {
                "sandbox_denied"
            } else if is_tls_failure(&failure.output) {
                "tls_error"
            } else if is_network_failure(&failure.output) {
                "network_error"
            } else if failure.exit_code == Some(127) {
                "command_not_found"
            } else if failure.exit_code == Some(126) {
                "command_not_executable"
            } else {
                "process_exit"
            },
            exit_code: failure.exit_code,
        };
    }
    audit_failure_text(&format!("{error:#}"))
}

pub(crate) fn audit_failure_text(output: &str) -> ToolFailure {
    let reason = if output.contains("outside every configured workspace")
        || output.contains("outside the session scope")
    {
        "workspace_denied"
    } else if is_sandbox_denial(output) {
        "sandbox_denied"
    } else if output.contains("timed out") {
        "timeout"
    } else if is_tls_failure(output) {
        "tls_error"
    } else if is_network_failure(output) {
        "network_error"
    } else if output.contains("must be")
        || output.contains("may not")
        || output.contains("unknown tool")
        || output.contains("invalid tool")
    {
        "invalid_arguments"
    } else if output.contains("could not start") || output.contains("could not launch") {
        "spawn_error"
    } else {
        "tool_error"
    };
    ToolFailure {
        reason,
        exit_code: None,
    }
}

fn is_tls_failure(output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    output.contains("native-tls")
        || output.contains("certificate verify failed")
        || output.contains("bad certificate format")
        || output.contains("invalid certificate")
}

fn is_network_failure(output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    output.contains("could not resolve host")
        || output.contains("connection refused")
        || output.contains("network is unreachable")
        || output.contains("failed to download")
}

#[cfg(feature = "control-plane")]
pub(crate) fn audit_change(tool: &str, success: bool, output: &str) -> Option<&'static str> {
    if !success {
        return None;
    }
    match tool {
        "write_file" if output.starts_with("Created ") => Some("created"),
        "write_file" | "edit_file" => Some("modified"),
        "delete_file" => Some("deleted"),
        _ => None,
    }
}

#[cfg(any(feature = "control-plane", test))]
pub(crate) fn audit_decision(success: bool, failure: Option<&ToolFailure>) -> &'static str {
    if success {
        "allowed"
    } else if failure
        .is_some_and(|failure| matches!(failure.reason, "workspace_denied" | "sandbox_denied"))
    {
        "denied"
    } else {
        "error"
    }
}

fn operation_name(tool: &str) -> &str {
    match tool {
        "sandbox_info" => "sandbox_info",
        "read_file" => "file_read",
        "list_directory" => "directory_list",
        "write_file" => "file_write",
        "search_files" => "file_search",
        "inspect_code" => "code_index",
        "batch" => "batch",
        "edit_file" => "file_edit",
        "delete_file" => "file_delete",
        "run_command" | "exec" => "command",
        _ => "tool",
    }
}

fn command_context(context: &mut ToolContext, arguments: &Value, workspace: &Path) {
    let Some(program) = arguments.get("program").and_then(Value::as_str) else {
        return;
    };
    let args = arguments["args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if matches!(program, "sh" | "bash" | "zsh") && args.first().is_some_and(|arg| arg == "-c") {
        let parsed = args.get(1).and_then(|body| simple_shell_argv(body));
        let command = parsed
            .as_ref()
            .and_then(|argv| argv.first())
            .map(String::as_str)
            .unwrap_or("shell");
        let command = Path::new(command)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("shell")
            .to_owned();
        context.command = Some(command.clone());
        context.argv_display = parsed.map_or_else(
            || vec![command.clone(), "<shell-command>".to_owned()],
            |mut argv| {
                argv = nono::scrub_argv(&argv);
                if let Some(program) = argv.first_mut() {
                    *program = command.clone();
                }
                for argument in argv.iter_mut().skip(1) {
                    *argument = sanitize_audit_argument(argument, workspace);
                }
                argv
            },
        );
    } else {
        let mut argv = vec![program.to_owned()];
        argv.extend(args);
        let command = Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("command")
            .to_owned();
        let mut argv = nono::scrub_argv(&argv);
        if let Some(program) = argv.first_mut() {
            *program = command.clone();
        }
        for argument in argv.iter_mut().skip(1) {
            *argument = sanitize_audit_argument(argument, workspace);
        }
        context.command = Some(command);
        context.argv_display = argv;
    }
    if let Some(cwd) = arguments.get("cwd").and_then(Value::as_str) {
        context.cwd_display = Some(display_audit_path(workspace, Path::new(cwd)));
    }
}

fn simple_shell_argv(command: &str) -> Option<Vec<String>> {
    let mut quote = None;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            }
            continue;
        }
        if quote.is_none() && matches!(character, '|' | '&' | ';' | '<' | '>' | '$' | '`' | '\n') {
            return None;
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    let argv = shell_words::split(command).ok()?;
    let program = argv.first()?;
    if program.contains('=') || !auditable_program(program) {
        return None;
    }
    Some(argv)
}

fn auditable_program(program: &str) -> bool {
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    matches!(
        program,
        "cargo"
            | "rustc"
            | "rustup"
            | "git"
            | "make"
            | "just"
            | "npm"
            | "npx"
            | "pnpm"
            | "yarn"
            | "bun"
            | "node"
            | "python"
            | "python3"
            | "pytest"
            | "uv"
            | "ruff"
            | "go"
            | "gofmt"
            | "pwd"
    )
}

fn sanitize_audit_argument(argument: &str, workspace: &Path) -> String {
    if Path::new(argument).is_absolute() {
        return display_audit_path(workspace, Path::new(argument));
    }
    if let Some((name, value)) = argument.split_once('=')
        && Path::new(value).is_absolute()
    {
        return format!("{name}={}", display_audit_path(workspace, Path::new(value)));
    }
    argument.to_owned()
}

fn display_audit_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace).map_or_else(
        |_| path_hash(path),
        |relative| {
            if relative.as_os_str().is_empty() {
                ".".to_owned()
            } else {
                relative.to_string_lossy().into_owned()
            }
        },
    )
}

fn path_hash(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut hasher = Sha256::new();
    hasher.update(b"nonaut.audit.path.v1\n");
    hasher.update(path.as_os_str().as_bytes());
    format!("sha256:{}", hex(&hasher.finalize()))
}

fn workspace_hash(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut hasher = Sha256::new();
    hasher.update(b"nonaut.audit.workspace.v1\n");
    hasher.update(path.as_os_str().as_bytes());
    format!("sha256:{}", hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub async fn execute(workspace: &Path, tool: &str, arguments: &Value) -> Result<String> {
    execute_with_progress(workspace, tool, arguments, None).await
}

pub(crate) async fn execute_with_progress(
    workspace: &Path,
    tool: &str,
    arguments: &Value,
    progress: Option<ProgressReporter>,
) -> Result<String> {
    if tool == "sandbox_info" {
        if arguments
            .as_object()
            .is_none_or(|arguments| !arguments.is_empty())
        {
            bail!("sandbox_info does not accept arguments");
        }
        return crate::policy::pin(workspace)?.capability_manifest_json();
    }
    if tool == "batch" {
        return execute_batch(workspace, arguments).await;
    }
    let arguments = normalize_tool_paths(workspace, tool, arguments)?;
    let (operation, fallback_root, access) = operation_location(workspace, tool, &arguments)?;
    let policy = crate::policy::pin(workspace)?;
    policy.authorize(&operation, &fallback_root, access)?;
    let operation_workspace = policy.workspace.clone();
    if access == OperationAccess::Write {
        crate::undo::checkpoint_before_mutation(workspace).await;
    }
    sandbox::execute(&operation_workspace, tool, &arguments, &policy, progress).await
}

pub(crate) async fn prewarm(workspace: &Path) -> Result<()> {
    let policy = crate::policy::pin(workspace)?;
    sandbox::prewarm(&policy.workspace, &policy).await
}

async fn execute_batch(workspace: &Path, arguments: &Value) -> Result<String> {
    let calls = arguments["calls"]
        .as_array()
        .context("batch calls must be an array")?;
    if calls.is_empty() || calls.len() > 16 {
        bail!("batch calls must contain between 1 and 16 items");
    }
    let mut validated = Vec::with_capacity(calls.len());
    for call in calls {
        let tool = required_string(call, "tool")?;
        if !matches!(
            tool,
            "read_file" | "list_directory" | "search_files" | "inspect_code"
        ) {
            bail!("batch supports read-only tools; invalid tool `{tool}`");
        }
        let call_arguments = call
            .get("arguments")
            .context("batch call arguments are required")?;
        validated.push((tool, call_arguments));
    }
    let policy = crate::policy::pin(workspace)?;
    let mut normalized = Vec::with_capacity(validated.len());
    for (tool, call_arguments) in validated {
        let call_arguments = normalize_tool_paths(workspace, tool, call_arguments)?;
        let (operation, fallback_root, access) =
            operation_location(workspace, tool, &call_arguments)?;
        policy.authorize(&operation, &fallback_root, access)?;
        normalized.push(json!({"tool": tool, "arguments": call_arguments}));
    }
    sandbox::execute(
        &policy.workspace,
        "batch",
        &json!({"calls": normalized}),
        &policy,
        None,
    )
    .await
}

pub(crate) async fn execute_shell(
    session_root: &Path,
    cwd: &Path,
    command: String,
    progress: Option<ProgressReporter>,
) -> Result<String> {
    execute_with_progress(
        session_root,
        "run_command",
        &json!({
            "program": "sh",
            "args": ["-c", command],
            "cwd": cwd,
        }),
        progress,
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
    let access = if matches!(
        tool,
        "read_file" | "list_directory" | "search_files" | "inspect_code"
    ) {
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
            | "inspect_code"
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
    status_result(name, output.status, combined)
}

fn status_result(name: &str, status: std::process::ExitStatus, output: String) -> Result<String> {
    let combined = truncate(&output);
    if status.success() {
        if combined.trim().is_empty() {
            Ok(format!("{name} completed successfully"))
        } else {
            Ok(combined)
        }
    } else {
        Err(ProcessFailure {
            name: name.to_owned(),
            status: status.to_string(),
            exit_code: status.code(),
            denied: is_sandbox_denial(&combined),
            output: combined,
        }
        .into())
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
    let line_count = value.lines().count();
    if value.len() <= MAX_OUTPUT_BYTES && line_count <= MAX_OUTPUT_LINES {
        return value.to_owned();
    }
    let head_limit = MAX_OUTPUT_BYTES * 3 / 4;
    let tail_limit = MAX_OUTPUT_BYTES - head_limit;
    let mut head = 0;
    for (lines, line) in value.split_inclusive('\n').enumerate() {
        if lines >= MAX_OUTPUT_LINES * 3 / 4 || head + line.len() > head_limit {
            break;
        }
        head += line.len();
    }
    let mut tail = value.len();
    for (tail_lines, line) in value.lines().rev().enumerate() {
        let additional = line.len().saturating_add(1);
        if tail_lines >= MAX_OUTPUT_LINES / 4 || value.len() - tail + additional > tail_limit {
            break;
        }
        tail = tail.saturating_sub(additional);
    }
    while !value.is_char_boundary(head) {
        head -= 1;
    }
    while tail < value.len() && !value.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[output truncated by Nonaut: middle omitted]\n{}",
        value[..head].trim_end(),
        value[tail..].trim_start()
    )
}

pub fn run_sandbox() -> Result<()> {
    sandbox::run()
}

fn execute_tool_local(workspace: &Path, tool: &str, arguments: &Value) -> Result<String> {
    match tool {
        "batch" => execute_batch_local(workspace, arguments),
        "read_file" => read_file(workspace, arguments),
        "list_directory" => list_directory(workspace, arguments),
        "write_file" => write_file(workspace, arguments),
        "search_files" => search_files(workspace, arguments),
        "inspect_code" => index::inspect(workspace, arguments),
        "edit_file" => edit_file(workspace, arguments),
        "delete_file" => delete_file(workspace, arguments),
        _ => bail!("unknown tool `{tool}`"),
    }
}

fn execute_batch_local(workspace: &Path, arguments: &Value) -> Result<String> {
    let calls = arguments["calls"]
        .as_array()
        .context("batch calls must be an array")?;
    let results = std::thread::scope(|scope| {
        calls
            .iter()
            .map(|call| {
                scope.spawn(move || {
                    let tool = required_string(call, "tool")?;
                    let arguments = call
                        .get("arguments")
                        .context("batch call arguments are required")?;
                    execute_tool_local(workspace, tool, arguments)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| {
                thread
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("batch read panicked")))
            })
            .collect::<Vec<_>>()
    });
    let mut output = String::new();
    for (index, (call, result)) in calls.iter().zip(results).enumerate() {
        if !output.is_empty() {
            output.push_str("\n\n");
        }
        let tool = required_string(call, "tool")?;
        output.push_str(&format!("[{}] {tool}\n", index + 1));
        match result {
            Ok(value) => output.push_str(&value),
            Err(error) => output.push_str(&format!("Error: {error:#}")),
        }
    }
    Ok(truncate(&output))
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
    fn shell_audit_context_excludes_the_command_body() {
        let mut context = ToolContext::minimal("call-1", "run_command");
        command_context(
            &mut context,
            &json!({
                "program": "sh",
                "args": ["-c", "cargo test --token very-secret"],
                "cwd": "/workspace"
            }),
            Path::new("/workspace"),
        );
        assert_eq!(context.command.as_deref(), Some("cargo"));
        assert_eq!(
            context.argv_display,
            ["cargo", "test", "--token", "[REDACTED]"]
        );
        assert_eq!(context.cwd_display.as_deref(), Some("."));
        assert!(
            !serde_json::to_string(&context)
                .unwrap()
                .contains("very-secret")
        );
    }

    #[test]
    fn complex_shell_audit_context_hides_the_command_body() {
        let mut context = ToolContext::minimal("call-1", "run_command");
        command_context(
            &mut context,
            &json!({
                "program": "sh",
                "args": ["-c", "cargo test && curl https://example.test/private"],
                "cwd": "/workspace"
            }),
            Path::new("/workspace"),
        );
        assert_eq!(context.command.as_deref(), Some("shell"));
        assert_eq!(context.argv_display, ["shell", "<shell-command>"]);
        assert!(!serde_json::to_string(&context).unwrap().contains("private"));
    }

    #[test]
    fn process_failure_preserves_the_exit_code_for_audit() {
        let output = std::process::Command::new("sh")
            .args(["-c", "exit 37"])
            .output()
            .unwrap();
        let error = output_result("test command", output).unwrap_err();
        let failure = audit_failure(&error);
        assert_eq!(failure.reason, "process_exit");
        assert_eq!(failure.exit_code, Some(37));
        assert_eq!(audit_decision(false, Some(&failure)), "error");
    }

    #[test]
    fn missing_command_has_a_specific_audit_reason() {
        let output = std::process::Command::new("sh")
            .args(["-c", "exit 127"])
            .output()
            .unwrap();
        let error = output_result("test command", output).unwrap_err();
        let failure = audit_failure(&error);
        assert_eq!(failure.reason, "command_not_found");
        assert_eq!(failure.exit_code, Some(127));
    }

    #[test]
    fn tls_and_network_failures_are_distinguished_from_process_errors() {
        assert_eq!(
            audit_failure_text("download error: native-tls: bad certificate format").reason,
            "tls_error"
        );
        assert_eq!(
            audit_failure_text("failed to download archive: network is unreachable").reason,
            "network_error"
        );
    }

    #[test]
    fn external_audit_paths_are_opaque() {
        let shown = display_audit_path(
            Path::new("/Users/example/Work/nonaut"),
            Path::new("/Users/example/.ssh/config"),
        );
        assert!(shown.starts_with("sha256:"));
        assert!(!shown.contains("example"));
    }

    #[test]
    fn command_audit_context_hides_external_absolute_paths() {
        let mut context = ToolContext::minimal("call-1", "run_command");
        command_context(
            &mut context,
            &json!({
                "program": "/usr/bin/cargo",
                "args": ["--manifest-path=/Users/example/private/Cargo.toml"],
                "cwd": "/Users/example/private"
            }),
            Path::new("/workspace"),
        );
        let encoded = serde_json::to_string(&context).unwrap();
        assert_eq!(context.command.as_deref(), Some("cargo"));
        assert!(!encoded.contains("/Users/example"));
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
    fn oversized_output_keeps_diagnostics_at_both_ends() {
        let value = format!("START\n{}\nEND\n", "middle\n".repeat(MAX_OUTPUT_LINES + 50));
        let output = truncate(&value);
        assert!(output.starts_with("START\n"));
        assert!(output.contains("middle omitted"));
        assert!(output.ends_with("END\n"));
    }

    #[tokio::test]
    async fn batch_rejects_mutations_before_execution() {
        let error = execute_batch(
            Path::new("/unused"),
            &json!({"calls": [{"tool": "write_file", "arguments": {}}]}),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("read-only"));
    }

    #[test]
    fn local_batch_preserves_parallel_read_order() {
        let root = std::env::temp_dir().join(format!("nonaut-batch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("first.txt"), "first").unwrap();
        fs::write(root.join("second.txt"), "second").unwrap();
        let output = execute_batch_local(
            &root,
            &json!({"calls": [
                {"tool": "read_file", "arguments": {"path": root.join("first.txt")}},
                {"tool": "read_file", "arguments": {"path": root.join("second.txt")}}
            ]}),
        )
        .unwrap();
        assert!(output.find("first").unwrap() < output.find("second").unwrap());
        fs::remove_dir_all(root).unwrap();
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
                "sandbox_info",
                "read_file",
                "list_directory",
                "write_file",
                "search_files",
                "inspect_code",
                "batch",
                "edit_file",
                "delete_file",
                "run_command"
            ]
        );
    }
}
