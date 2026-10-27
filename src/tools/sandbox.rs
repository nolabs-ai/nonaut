use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result, bail};
use nono::{AccessMode, CapabilitySet, Sandbox};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::policy::Network;

const PROXY_PORT_ENV: &str = "NONAUT_PROXY_PORT";
const NETWORK_BLOCKED_ENV: &str = "NONAUT_NETWORK_BLOCKED";
const SANDBOX_RUNTIME_ENV: &str = "NONAUT_SANDBOX_RUNTIME";

#[derive(Debug, Serialize, Deserialize)]
struct Runtime {
    read_paths: Vec<PathBuf>,
    write_paths: Vec<PathBuf>,
    path_entries: Vec<PathBuf>,
    home: PathBuf,
    environment: BTreeMap<String, String>,
}

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
        "tool" => {
            let tool = arguments
                .next()
                .and_then(|value| value.into_string().ok())
                .context("sandboxed tool name was not provided")?;
            super::run_tool_worker(&workspace, &tool)
        }
        "command" => run_command(&workspace, arguments.collect(), &runtime),
        _ => bail!("unknown sandbox mode `{mode}`"),
    }
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

#[cfg(unix)]
fn run_command(workspace: &Path, arguments: Vec<OsString>, runtime: &Runtime) -> Result<()> {
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
    let command_path = runtime.command_path();
    command
        .args(arguments)
        .current_dir(workdir)
        .env_clear()
        .env("PATH", command_path)
        .env("HOME", &runtime.home)
        .env("TMPDIR", workspace)
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .envs(&runtime.environment);
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
fn run_command(_workspace: &Path, _arguments: Vec<OsString>, _runtime: &Runtime) -> Result<()> {
    bail!("sandboxed commands are only supported on Unix platforms")
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
