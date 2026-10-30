use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result, bail};
use nono::{AccessMode, CapabilitySet, Sandbox};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::{Duration, MissedTickBehavior, interval, timeout},
};

use crate::policy::Network;

const PROXY_PORT_ENV: &str = "NONAUT_PROXY_PORT";
const NETWORK_BLOCKED_ENV: &str = "NONAUT_NETWORK_BLOCKED";
const SANDBOX_RUNTIME_ENV: &str = "NONAUT_SANDBOX_RUNTIME";
const CAPABILITY_FILE_ENV: &str = "NONAUT_CAP_FILE";

#[derive(Debug, Serialize, Deserialize)]
struct Runtime {
    read_paths: Vec<PathBuf>,
    write_paths: Vec<PathBuf>,
    path_entries: Vec<PathBuf>,
    home: PathBuf,
    environment: BTreeMap<String, String>,
    capability_file: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct WorkerRequest {
    tool: String,
    arguments: Value,
}

#[derive(Serialize, Deserialize)]
struct WorkerResponse {
    output: Option<String>,
    error: Option<String>,
    failure: Option<WorkerFailure>,
}

#[derive(Serialize, Deserialize)]
struct WorkerFailure {
    name: String,
    status: String,
    exit_code: Option<i32>,
    output: String,
    denied: bool,
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
}

struct CallGuard {
    pid: Option<u32>,
    armed: bool,
}

struct CaptureFile(Option<PathBuf>);

type WorkerPool = HashMap<PathBuf, Arc<Mutex<Worker>>>;

static WORKERS: OnceLock<Mutex<WorkerPool>> = OnceLock::new();
static NEXT_CAPTURE: AtomicU64 = AtomicU64::new(0);

impl Runtime {
    fn from_policy(policy: &crate::policy::Policy) -> Self {
        let base = crate::sandbox::platform::command_path();
        let path_entries = policy
            .path_entries
            .iter()
            .cloned()
            .chain(std::env::split_paths(&base))
            .collect();
        Self {
            read_paths: policy.read_paths.clone(),
            write_paths: policy.write_paths.clone(),
            path_entries,
            home: policy.command_home.clone(),
            environment: policy.environment.clone(),
            capability_file: policy
                .capability_file
                .clone()
                .expect("pinned policy has a capability file"),
        }
    }

    fn from_environment() -> Result<Self> {
        let encoded = std::env::var(SANDBOX_RUNTIME_ENV)
            .context("sandbox runtime configuration was not provided")?;
        serde_json::from_str(&encoded).context("invalid sandbox runtime configuration")
    }

    fn command_path(&self) -> String {
        std::env::join_paths(&self.path_entries)
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }
}

pub(super) fn process(
    workspace: &Path,
    executable: &Path,
    policy: &crate::policy::Policy,
) -> Result<Command> {
    let runtime = Runtime::from_policy(policy);
    let command_path = runtime.command_path();
    let encoded = serde_json::to_string(&runtime).context("could not encode sandbox runtime")?;
    let mut command = Command::new(executable);
    #[cfg(unix)]
    command.process_group(0);
    command
        .arg("__nonaut_sandbox")
        .env_clear()
        .env("PATH", &command_path)
        .env("HOME", &runtime.home)
        .env("TMPDIR", workspace)
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .env(CAPABILITY_FILE_ENV, &runtime.capability_file)
        .env(SANDBOX_RUNTIME_ENV, encoded)
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
    Ok(command)
}

/// Execute through the one immutable worker owned by this workspace session.
pub(super) async fn execute(
    workspace: &Path,
    tool: &str,
    arguments: &Value,
    policy: &crate::policy::Policy,
    progress: Option<super::ProgressReporter>,
) -> Result<String> {
    let worker = worker(workspace, policy).await?;

    let mut arguments = arguments.clone();
    let capture = CaptureFile((tool == "run_command").then(|| {
        policy.command_home.join(format!(
            ".nonaut-output-{}-{}",
            std::process::id(),
            NEXT_CAPTURE.fetch_add(1, Ordering::Relaxed)
        ))
    }));
    if let Some(path) = capture.0.as_ref() {
        arguments["_capture_path"] = Value::String(path.to_string_lossy().into_owned());
    }
    let seconds = arguments["timeout_seconds"]
        .as_u64()
        .unwrap_or(policy.tool_timeout_seconds)
        .clamp(1, policy.tool_timeout_seconds);
    let request = WorkerRequest {
        tool: tool.to_owned(),
        arguments,
    };
    let limit = Duration::from_secs(seconds);
    let progress_task = capture.0.as_ref().zip(progress).map(|(path, report)| {
        let path = path.clone();
        tokio::spawn(report_command_progress(path, report))
    });
    let response = {
        let mut worker = worker.lock().await;
        match timeout(limit, worker.call(&request)).await {
            Ok(result) => result,
            Err(_) => {
                let _ = worker.child.kill().await;
                let partial = capture
                    .0
                    .as_ref()
                    .and_then(|path| fs::read_to_string(path).ok())
                    .unwrap_or_default();
                let detail = if partial.trim().is_empty() {
                    String::new()
                } else {
                    format!("\nPartial output:\n{}", super::truncate(&partial))
                };
                Err(anyhow::anyhow!(
                    "tool timed out after {} seconds{}",
                    seconds,
                    detail
                ))
            }
        }
    };
    if let Some(task) = progress_task {
        task.abort();
    }
    if response.is_err() {
        WORKERS
            .get()
            .expect("worker pool initialized")
            .lock()
            .await
            .remove(workspace);
    }
    let response = response?;
    if let Some(failure) = response.failure {
        return Err(super::ProcessFailure {
            name: failure.name,
            status: failure.status,
            exit_code: failure.exit_code,
            output: failure.output,
            denied: failure.denied,
        }
        .into());
    }
    match (response.output, response.error) {
        (Some(output), None) => Ok(output),
        (None, Some(error)) => bail!(error),
        _ => bail!("sandbox worker returned an invalid response"),
    }
}

fn command_progress(output: &str) -> super::CommandProgress {
    let line_count = output.lines().count();
    let latest_line = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .chars()
        .take(160)
        .collect();
    super::CommandProgress {
        latest_line,
        line_count,
    }
}

async fn report_command_progress(path: PathBuf, report: super::ProgressReporter) {
    let mut updates = interval(Duration::from_millis(350));
    updates.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut previous = super::CommandProgress {
        latest_line: String::new(),
        line_count: 0,
    };
    loop {
        updates.tick().await;
        if let Ok(output) = fs::read_to_string(&path) {
            let current = command_progress(&output);
            if current != previous {
                report(current.clone());
                previous = current;
            }
        }
    }
}

pub(super) async fn prewarm(workspace: &Path, policy: &crate::policy::Policy) -> Result<()> {
    worker(workspace, policy).await.map(|_| ())
}

async fn worker(workspace: &Path, policy: &crate::policy::Policy) -> Result<Arc<Mutex<Worker>>> {
    let pool = WORKERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pool = pool.lock().await;
    if let Some(worker) = pool.get(workspace) {
        return Ok(Arc::clone(worker));
    }
    let worker = Arc::new(Mutex::new(spawn_worker(workspace, policy).await?));
    pool.insert(workspace.to_path_buf(), Arc::clone(&worker));
    Ok(worker)
}

async fn spawn_worker(workspace: &Path, policy: &crate::policy::Policy) -> Result<Worker> {
    let executable = std::env::current_exe().context("could not locate the Nonaut executable")?;
    let mut command = process(workspace, &executable, policy)?;
    command
        .arg("worker")
        .arg(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .context("could not start the sandbox worker")?;
    let stdin = child
        .stdin
        .take()
        .context("sandbox worker stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("sandbox worker stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("sandbox worker stderr unavailable")?;
    Ok(Worker {
        child,
        stdin,
        stdout: BufReader::new(stdout),
        stderr: BufReader::new(stderr),
    })
}

impl Worker {
    async fn call(&mut self, request: &WorkerRequest) -> Result<WorkerResponse> {
        let mut guard = CallGuard {
            pid: self.child.id(),
            armed: true,
        };
        let mut encoded = serde_json::to_vec(request).context("could not encode tool request")?;
        encoded.push(b'\n');
        self.stdin
            .write_all(&encoded)
            .await
            .context("could not send sandbox worker request")?;
        self.stdin
            .flush()
            .await
            .context("could not flush sandbox worker request")?;
        let mut response = Vec::new();
        let read = self
            .stdout
            .read_until(b'\n', &mut response)
            .await
            .context("could not read sandbox worker response")?;
        if read == 0 {
            let mut error = String::new();
            let _ = self.stderr.read_to_string(&mut error).await;
            if error.trim().is_empty() {
                bail!("sandbox worker stopped unexpectedly");
            }
            bail!("sandbox worker stopped: {}", error.trim());
        }
        let response =
            serde_json::from_slice(&response).context("invalid sandbox worker response")?;
        guard.armed = false;
        Ok(response)
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.armed
            && let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok())
        {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

impl Drop for CaptureFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

pub(crate) fn run() -> Result<()> {
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
    let runtime = Runtime::from_environment()?;
    let capabilities = capabilities(&workspace, &runtime)?;
    #[cfg(target_os = "linux")]
    {
        let fallback =
            Sandbox::apply_auto(&capabilities).context("could not apply the nono sandbox")?;
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
        "worker" => run_worker(&workspace, &runtime),
        _ => bail!("unknown sandbox mode `{mode}`"),
    }
}

fn run_worker(workspace: &Path, runtime: &Runtime) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let response = match line {
            Ok(line) => match serde_json::from_str::<WorkerRequest>(&line) {
                Ok(request) => worker_call(workspace, runtime, request),
                Err(error) => WorkerResponse {
                    output: None,
                    error: Some(format!("invalid sandbox worker request: {error}")),
                    failure: None,
                },
            },
            Err(error) => return Err(error).context("could not read sandbox worker request"),
        };
        serde_json::to_writer(&mut stdout, &response)
            .context("could not encode sandbox worker response")?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}

fn worker_call(workspace: &Path, runtime: &Runtime, request: WorkerRequest) -> WorkerResponse {
    let result = if request.tool == "run_command" {
        run_worker_command(workspace, runtime, &request.arguments)
    } else {
        super::execute_tool_local(workspace, &request.tool, &request.arguments)
    };
    match result {
        Ok(output) => WorkerResponse {
            output: Some(output),
            error: None,
            failure: None,
        },
        Err(error) => match error.downcast_ref::<super::ProcessFailure>() {
            Some(failure) => WorkerResponse {
                output: None,
                error: None,
                failure: Some(WorkerFailure {
                    name: failure.name.clone(),
                    status: failure.status.clone(),
                    exit_code: failure.exit_code,
                    output: failure.output.clone(),
                    denied: failure.denied,
                }),
            },
            None => WorkerResponse {
                output: None,
                error: Some(format!("{error:#}")),
                failure: None,
            },
        },
    }
}

fn run_worker_command(workspace: &Path, runtime: &Runtime, arguments: &Value) -> Result<String> {
    let program = super::required_string(arguments, "program")?;
    let args = arguments["args"]
        .as_array()
        .context("args must be an array")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .context("every command argument must be a string")
        })
        .collect::<Result<Vec<_>>>()?;
    let workdir = PathBuf::from(super::required_string(arguments, "cwd")?);
    let capture = PathBuf::from(super::required_string(arguments, "_capture_path")?);
    anyhow::ensure!(
        capture.parent() == Some(runtime.home.as_path()),
        "invalid command output capture path"
    );
    let output = fs::File::create(&capture).context("could not create command output capture")?;
    let error = output
        .try_clone()
        .context("could not clone command output capture")?;
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .current_dir(workdir)
        .env_clear()
        .env("PATH", runtime.command_path())
        .env("HOME", &runtime.home)
        .env("TMPDIR", workspace)
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .env(CAPABILITY_FILE_ENV, &runtime.capability_file)
        .envs(&runtime.environment)
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(error));
    if let Some(port) = proxy_port() {
        let proxy = format!("http://127.0.0.1:{port}");
        for name in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
            command.env(name, &proxy);
        }
    }
    let status = command
        .status()
        .with_context(|| format!("could not execute `{program}`"))?;
    let output = fs::read_to_string(&capture).context("command output was not UTF-8")?;
    super::status_result(program, status, output)
}

fn capabilities(workspace: &Path, runtime: &Runtime) -> Result<CapabilitySet> {
    let mut capabilities = match proxy_port() {
        Some(port) if landlock_filters_network() => CapabilitySet::new().proxy_only(port),
        Some(_) => {
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
    for path in &runtime.read_paths {
        capabilities = allow_existing(capabilities, path, AccessMode::Read)?;
    }
    for path in &runtime.write_paths {
        capabilities = allow_existing(capabilities, path, AccessMode::ReadWrite)?;
    }
    for path in crate::sandbox::platform::read_paths() {
        capabilities = allow_existing(capabilities, Path::new(path), AccessMode::Read)?;
    }
    for path in crate::sandbox::platform::write_paths() {
        capabilities = allow_existing(capabilities, Path::new(path), AccessMode::Write)?;
    }
    for path in &runtime.path_entries {
        capabilities = allow_existing(capabilities, path, AccessMode::Read)?;
    }
    capabilities
        .allow_path(workspace, AccessMode::ReadWrite)
        .context("could not grant workspace access")
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
        capabilities
            .allow_file(path, access)
            .with_context(|| format!("could not grant access to {}", path.display()))
    } else {
        Ok(capabilities)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_progress_reports_the_latest_meaningful_line() {
        assert_eq!(
            command_progress("Compiling one\n\nChecking two\n"),
            super::super::CommandProgress {
                latest_line: "Checking two".to_owned(),
                line_count: 3,
            }
        );
    }
}
