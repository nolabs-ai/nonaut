use std::{
    ffi::OsString,
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result, anyhow, bail};
use diffy::create_patch;
use directories::BaseDirs;
use nono::{AccessMode, CapabilitySet, Sandbox};

use crate::policy::Network;
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command};

const MAX_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DIFF_LINES: usize = 80;
const MAX_DIFF_BYTES: usize = 16 * 1024;

pub fn specs() -> Value {
    json!([
        {
            "type": "function",
            "name": "read_file",
            "description": "Attempt to read a UTF-8 text file, optionally limiting the inclusive line range. Pass the requested path to nono and let nono decide whether access is allowed; do not predict policy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to attempt. Relative paths start at the workspace; nono enforces access." },
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "list_directory",
            "description": "Attempt to list a directory. Pass the requested path to nono even when it appears outside the workspace; nono alone decides whether access is allowed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory path to attempt; use . for the workspace root. Nono enforces access." },
                    "depth": { "type": "integer", "minimum": 1, "maximum": 8 }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "write_file",
            "description": "Create or replace one UTF-8 text file and return a compact unified diff. Always call this tool for direct file creation or full-content replacement, including paths that appear outside the workspace. Do not predict policy; nono alone decides whether access is allowed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to attempt. Relative paths start at the workspace; Nonaut normalizes it and nono enforces access." },
                    "content": { "type": "string", "description": "Complete replacement file contents." }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "search_files",
            "description": "Attempt to search text files with ripgrep. The query is a regular expression. Pass the requested path to nono and let nono decide whether access is allowed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "path": { "type": "string", "description": "Search root to attempt; defaults to . and nono enforces access." },
                    "glob": { "type": "string", "description": "Optional ripgrep glob, such as *.rs." }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "edit_file",
            "description": "Edit a UTF-8 text file by replacing exact text and return a compact unified diff. Call this after at most one read when the user requests a focused patch or edit. Nono alone decides whether access is allowed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to attempt. Relative paths start at the workspace; nono enforces access." },
                    "old_text": { "type": "string", "description": "Exact existing text to replace." },
                    "new_text": { "type": "string", "description": "Replacement text." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of requiring exactly one; defaults to false." }
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "delete_file",
            "description": "Delete one file and return a compact unified diff. Always call this tool for an explicitly requested deletion, even when the path appears outside the workspace. Nono alone decides whether access is allowed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to attempt. Relative paths start at the workspace; nono enforces access." }
                },
                "required": ["path"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "run_command",
            "description": "Run a program under the policy selected by its explicit working directory. Always make an explicitly requested attempt and report the actual result instead of predicting a denial. The fresh nono sandbox grants only the matching workspace, its network policy, and no inherited secrets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "program": { "type": "string", "description": "Executable name, such as cargo, git, or sh." },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": "string", "description": "Working directory used to select the operation's workspace policy. Relative paths start at the session directory." },
                    "timeout_seconds": { "type": "integer", "minimum": 1, "description": "Optional; capped by the user's tool timeout (default 120 seconds)." }
                },
                "required": ["program", "args", "cwd"],
                "additionalProperties": false
            }
        }
    ])
}

pub async fn execute(workspace: &Path, tool: &str, arguments: &Value) -> Result<String> {
    let arguments = normalize_tool_paths(workspace, tool, arguments)?;
    let (operation, fallback_root) = operation_location(workspace, tool, &arguments)?;
    let policy = crate::policy::load(workspace, &operation, &fallback_root)?;
    let operation_workspace = policy.workspace.clone();
    if tool == "run_command" {
        return run_command(&operation_workspace, &arguments, &policy).await;
    }

    let executable = std::env::current_exe().context("could not locate the Nonaut executable")?;
    let mut child = sandbox_process(&operation_workspace, &executable, &policy)?;
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
) -> Result<(PathBuf, PathBuf)> {
    if tool == "run_command" {
        let cwd = PathBuf::from(required_string(arguments, "cwd")?);
        return Ok((cwd.clone(), cwd));
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
    Ok((path, fallback))
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
    let mut command = sandbox_process(workspace, &executable, policy)?;
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

// Set by the parent from the policy file; read by the sandbox child before
// it applies the sandbox. The child cannot change the parent's copy.
const PROXY_PORT_ENV: &str = "NONAUT_PROXY_PORT";
const NETWORK_BLOCKED_ENV: &str = "NONAUT_NETWORK_BLOCKED";
const READ_PATHS_ENV: &str = "NONAUT_READ_PATHS";
const COMMAND_PATH_ENV: &str = "NONAUT_COMMAND_PATH";
const COMMAND_HOME_ENV: &str = "NONAUT_COMMAND_HOME";

fn sandbox_process(
    workspace: &Path,
    executable: &Path,
    policy: &crate::policy::Policy,
) -> Result<Command> {
    let mut command = Command::new(executable);
    // Its own process group, so a timeout or interrupt can stop everything
    // the tool started, not just the direct child.
    #[cfg(unix)]
    command.process_group(0);
    command
        .arg("__nonaut_sandbox")
        .env_clear()
        .env("PATH", policy_path(policy))
        .env("HOME", &policy.command_home)
        .env("TMPDIR", workspace)
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .current_dir(workspace);
    match policy.network {
        Network::Open => {}
        Network::Blocked => {
            command.env(NETWORK_BLOCKED_ENV, "1");
        }
        Network::Allowlist => {
            let port = crate::proxy::ensure(&policy.allow_hosts)?;
            command.env(PROXY_PORT_ENV, port.to_string());
        }
    }
    if !policy.read_paths.is_empty() {
        let paths = policy
            .read_paths
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        command.env(READ_PATHS_ENV, paths);
    }
    command
        .env(COMMAND_PATH_ENV, policy_path(policy))
        .env(COMMAND_HOME_ENV, &policy.command_home);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn landlock_filters_network() -> bool {
    nono::sandbox::detect_abi().is_ok_and(|abi| abi.has_network())
}

#[cfg(not(target_os = "linux"))]
fn landlock_filters_network() -> bool {
    true
}

fn proxy_port() -> Option<u16> {
    std::env::var(PROXY_PORT_ENV).ok()?.parse().ok()
}

fn safe_path() -> String {
    let entries = [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/bin",
        "/sbin",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|entry| entry.is_dir());
    std::env::join_paths(entries)
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn policy_path(policy: &crate::policy::Policy) -> String {
    let base = safe_path();
    let entries = policy
        .path_entries
        .iter()
        .cloned()
        .chain(base.split(':').map(PathBuf::from));
    std::env::join_paths(entries)
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn command_path() -> String {
    std::env::var(COMMAND_PATH_ENV).unwrap_or_else(|_| safe_path())
}

fn command_home(workspace: &Path) -> PathBuf {
    std::env::var_os(COMMAND_HOME_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.to_path_buf())
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
    let mut arguments = std::env::args_os().skip(2);
    let mode = arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .context("sandbox mode was not provided")?;
    let workspace = PathBuf::from(
        arguments
            .next()
            .context("sandbox workspace was not provided")?,
    );
    let capabilities = sandbox_capabilities(&workspace)?;
    #[cfg(target_os = "linux")]
    {
        let fallback =
            Sandbox::apply_auto(&capabilities).context("could not apply the nono sandbox")?;
        // A proxy-only fallback needs a seccomp supervisor nonaut does not
        // run; refuse rather than continue with network unrestricted.
        anyhow::ensure!(
            !matches!(
                fallback,
                nono::sandbox::SeccompNetFallback::ProxyOnly { .. }
            ),
            "the nono sandbox could not restrict network on this kernel"
        );
    }
    #[cfg(not(target_os = "linux"))]
    Sandbox::apply_auto(&capabilities).context("could not apply the nono sandbox")?;

    match mode.as_str() {
        "tool" => {
            let tool = arguments
                .next()
                .and_then(|value| value.into_string().ok())
                .context("sandboxed tool name was not provided")?;
            run_tool_worker(&workspace, &tool)
        }
        "command" => run_sandboxed_command(&workspace, arguments.collect()),
        _ => bail!("unknown sandbox mode `{mode}`"),
    }
}

fn sandbox_capabilities(workspace: &Path) -> Result<CapabilitySet> {
    // Network is open unless the policy blocks it or limits it to nonaut's
    // filtering proxy.
    let mut capabilities = match proxy_port() {
        Some(port) if landlock_filters_network() => CapabilitySet::new().proxy_only(port),
        Some(_) => {
            // Without Landlock network rules (Linux < 6.7) nono cannot pin
            // connections to the proxy port, so fail closed.
            eprintln!(
                "nonaut: this kernel cannot limit network to the policy proxy \
                 (needs Linux 6.7+); network stays blocked"
            );
            CapabilitySet::new().block_network()
        }
        None if std::env::var_os(NETWORK_BLOCKED_ENV).is_some() => {
            CapabilitySet::new().block_network()
        }
        None => CapabilitySet::new(),
    };
    for path in std::env::var(READ_PATHS_ENV).unwrap_or_default().lines() {
        capabilities = allow_existing(capabilities, Path::new(path), AccessMode::Read)?;
    }
    for path in system_read_paths() {
        capabilities = allow_existing(capabilities, Path::new(path), AccessMode::Read)?;
    }
    for path in system_write_paths() {
        capabilities = allow_existing(capabilities, Path::new(path), AccessMode::Write)?;
    }
    for path in command_path().split(':') {
        capabilities = allow_existing(capabilities, Path::new(path), AccessMode::Read)?;
    }
    capabilities = capabilities
        .allow_path(workspace, AccessMode::ReadWrite)
        .context("could not grant workspace access")?;
    Ok(capabilities)
}

fn allow_existing(
    capabilities: CapabilitySet,
    path: &Path,
    access: AccessMode,
) -> Result<CapabilitySet> {
    if path.is_dir() {
        capabilities
            .allow_path(path, access)
            .with_context(|| format!("could not grant access to {}", path.display()))
    } else if fs::canonicalize(path).is_ok() {
        // /dev/stdin etc. resolve to pipe:[N] when stdio is piped, which nono
        // cannot canonicalize. Inherited descriptors need no grant, so skip.
        capabilities
            .allow_file(path, access)
            .with_context(|| format!("could not grant access to {}", path.display()))
    } else {
        Ok(capabilities)
    }
}

#[cfg(target_os = "macos")]
fn system_read_paths() -> &'static [&'static str] {
    &[
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/local/bin",
        "/usr/lib",
        "/usr/local/lib",
        "/usr/share",
        "/System/Library",
        "/Library",
        "/dev",
        "/private/var/db",
        "/private/etc",
        "/usr/share/zoneinfo",
        "/usr/share/locale",
        "/var/db/timezone",
        "/usr/share/terminfo",
        "/var",
        "/private/var",
        "/private",
        "/tmp",
        "/System/Volumes",
        "/System/Cryptexes",
        "/Applications",
        "/cores",
        "/opt",
        "/run",
        "/nix",
    ]
}

#[cfg(target_os = "linux")]
fn system_read_paths() -> &'static [&'static str] {
    &[
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/local/bin",
        "/lib",
        "/lib64",
        "/lib/x86_64-linux-gnu",
        "/lib/aarch64-linux-gnu",
        "/usr/lib",
        "/usr/lib64",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/usr/local/lib",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/gai.conf",
        "/etc/group",
        "/etc/ssl",
        "/etc/pki",
        "/etc/ca-certificates",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/etc/localtime",
        "/etc/timezone",
        "/etc/alternatives",
        "/etc/os-release",
        "/etc/passwd",
        "/etc/services",
        "/usr/share",
        "/usr/share/locale",
        "/usr/share/zoneinfo",
        "/usr/share/terminfo",
        "/usr/share/ca-certificates",
        "/lib/terminfo",
        "/etc/terminfo",
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/full",
        "/dev/tty",
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/fd",
        "/dev/pts",
        "/proc/self",
        "/proc/cpuinfo",
        "/proc/meminfo",
        "/proc/stat",
        "/proc/loadavg",
        "/proc/version",
        "/proc/filesystems",
    ]
}

#[cfg(target_os = "macos")]
fn system_write_paths() -> &'static [&'static str] {
    &[
        "/private/tmp",
        "/tmp",
        "/private/var/folders",
        "/var/folders",
        "/dev",
    ]
}

#[cfg(target_os = "linux")]
fn system_write_paths() -> &'static [&'static str] {
    &[
        "/tmp",
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/tty",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/fd",
        "/dev/pts",
        "/proc/self/fd",
    ]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_read_paths() -> &'static [&'static str] {
    &[]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn system_write_paths() -> &'static [&'static str] {
    &[]
}

#[cfg(unix)]
fn run_sandboxed_command(_workspace: &Path, arguments: Vec<OsString>) -> Result<()> {
    let mut arguments = arguments.into_iter();
    let workdir = PathBuf::from(
        arguments
            .next()
            .context("sandboxed command working directory was not provided")?,
    );
    let program = arguments
        .next()
        .context("sandboxed command program was not provided")?;
    let mut command = std::process::Command::new(&program);
    command
        .args(arguments)
        .current_dir(workdir)
        .env_clear()
        .env("PATH", command_path())
        .env("HOME", command_home(_workspace))
        .env("TMPDIR", _workspace)
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb");
    if let Some(port) = proxy_port() {
        let proxy = format!("http://127.0.0.1:{port}");
        for name in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
            command.env(name, &proxy);
        }
    }
    let error = command.exec();
    Err(error).with_context(|| format!("could not execute `{}`", program.to_string_lossy()))
}

#[cfg(not(unix))]
fn run_sandboxed_command(_workspace: &Path, _arguments: Vec<OsString>) -> Result<()> {
    bail!("sandboxed commands are only supported on Unix platforms")
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
