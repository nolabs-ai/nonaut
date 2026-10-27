//! Sandbox settings from Nonaut's unified `config.json`.
//!
//! The file lives outside the workspace because sandboxed tools may write the
//! workspace and must not be able to widen their own permissions.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use nono::net_filter::validate_host_pattern;

pub(crate) use crate::config::NetworkMode as Network;
use crate::config::{
    Config, FilesystemConfig, NetworkConfig, UnconfiguredWorkspaces, WorkspaceToolsConfig,
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Policy {
    /// Canonical directory granted read/write access for this operation.
    pub workspace: PathBuf,
    pub network: Network,
    /// Upper bound for any tool call, `run_command` and `/exec`.
    pub tool_timeout_seconds: u64,
    pub allow_hosts: Vec<String>,
    pub read_paths: Vec<PathBuf>,
    pub path_entries: Vec<PathBuf>,
    pub command_home: PathBuf,
}

pub(crate) const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 120;
const MAX_TOOL_TIMEOUT_SECONDS: u64 = 3600;

impl Default for Policy {
    fn default() -> Self {
        Self {
            workspace: PathBuf::new(),
            network: Network::Blocked,
            tool_timeout_seconds: DEFAULT_TOOL_TIMEOUT_SECONDS,
            allow_hosts: Vec::new(),
            read_paths: Vec::new(),
            path_entries: Vec::new(),
            command_home: PathBuf::new(),
        }
    }
}

pub(crate) fn load(session_root: &Path, operation: &Path, fallback_root: &Path) -> Result<Policy> {
    let config = crate::config::load()?;
    let home = BaseDirs::new()
        .context("could not determine the home directory")?
        .home_dir()
        .to_path_buf();
    resolve(config, session_root, operation, fallback_root, &home).with_context(|| {
        crate::config::config_path().map_or_else(
            |_| "invalid Nonaut configuration".to_owned(),
            |path| format!("invalid {}", path.display()),
        )
    })
}

fn resolve(
    config: Config,
    session_root: &Path,
    operation: &Path,
    fallback_root: &Path,
    home: &Path,
) -> Result<Policy> {
    let session_root = canonicalize_nearest(session_root)?;
    let operation = canonicalize_nearest(operation)?;
    let fallback_root = canonicalize_nearest(fallback_root)?;
    let mut network = config.sandbox.network;
    let mut filesystem = config.sandbox.filesystem;
    let mut tools = WorkspaceToolsConfig {
        timeout_seconds: Some(config.tools.timeout_seconds),
    };

    let matching = config
        .sandbox
        .workspaces
        .iter()
        .filter_map(|(path, settings)| {
            let path = expand_home(&path.to_string_lossy(), home);
            let path = canonicalize_nearest(&path).ok()?;
            operation.starts_with(&path).then_some((path, settings))
        })
        .max_by_key(|(path, _)| path.components().count());

    let (workspace, settings) = if let Some((path, settings)) = matching {
        (path, Some(settings))
    } else {
        if config.sandbox.unconfigured_workspaces == UnconfiguredWorkspaces::Deny {
            bail!(
                "operation at {} is outside every configured workspace",
                operation.display()
            );
        }
        if !operation.starts_with(&session_root) {
            bail!(
                "operation at {} is outside the session scope {}",
                operation.display(),
                session_root.display()
            );
        }
        (fallback_root, None)
    };

    let mut profile = None;
    if let Some(extra) = settings {
        profile = extra.profile.as_deref();
        if let Some(extra_network) = &extra.network {
            network = extra_network.clone();
        }
        if let Some(extra_filesystem) = &extra.filesystem {
            filesystem
                .read
                .extend(extra_filesystem.read.iter().cloned());
        }
        if let Some(timeout) = extra
            .tools
            .as_ref()
            .and_then(|settings| settings.timeout_seconds)
        {
            tools.timeout_seconds = Some(timeout);
        }
    }

    validate(workspace, network, filesystem, tools, profile, home)
}

fn validate(
    workspace: PathBuf,
    network: NetworkConfig,
    filesystem: FilesystemConfig,
    tools: WorkspaceToolsConfig,
    profile: Option<&str>,
    home: &Path,
) -> Result<Policy> {
    let network_mode = match (network.mode, network.allow_domains.is_empty()) {
        (None, true) => Network::Blocked,
        (None, false) | (Some(Network::Allowlist), false) => Network::Allowlist,
        (Some(Network::Allowlist), true) => {
            bail!("sandbox.network.mode = allowlist needs at least one allow_domains entry")
        }
        (Some(mode), false) => {
            let mode = match mode {
                Network::Open => "open",
                Network::Blocked => "blocked",
                Network::Allowlist => unreachable!(),
            };
            bail!("allow_domains only applies with sandbox.network.mode = allowlist, not {mode}")
        }
        (Some(mode), true) => mode,
    };
    let tool_timeout_seconds = tools
        .timeout_seconds
        .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECONDS);
    if !(1..=MAX_TOOL_TIMEOUT_SECONDS).contains(&tool_timeout_seconds) {
        bail!("tools.timeout_seconds must be between 1 and {MAX_TOOL_TIMEOUT_SECONDS}");
    }
    for host in &network.allow_domains {
        if host == "*" {
            bail!("allow_domains may not contain \"*\"; list the domains the sandbox needs");
        }
        validate_host_pattern(host).map_err(anyhow::Error::msg)?;
    }
    let mut read_paths = filesystem
        .read
        .iter()
        .map(|raw| {
            let path = expand_home(raw, home);
            if path.is_absolute() {
                Ok(path)
            } else {
                bail!("sandbox.filesystem.read entry `{raw}` must be absolute or start with ~/")
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let mut path_entries = Vec::new();
    let mut command_home = workspace.clone();
    match profile {
        None | Some("default") => {}
        Some("rust-dev") => {
            let cargo = home.join(".cargo");
            for path in [
                cargo.join("bin"),
                cargo.join("registry"),
                cargo.join("git"),
                home.join(".rustup"),
            ] {
                if path.exists() {
                    read_paths.push(path);
                }
            }
            path_entries.push(cargo.join("bin"));
            command_home = home.to_path_buf();
        }
        Some(profile) => bail!("unsupported workspace profile `{profile}`"),
    }
    Ok(Policy {
        workspace,
        network: network_mode,
        tool_timeout_seconds,
        allow_hosts: network.allow_domains,
        read_paths,
        path_entries,
        command_home,
    })
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(|| format!("could not canonicalize {}", path.display()))
}

/// Canonicalize the existing prefix while retaining a not-yet-created suffix.
fn canonicalize_nearest(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .context("operation path has no existing ancestor")?;
        suffix.push(name.to_os_string());
        existing = existing
            .parent()
            .context("operation path has no existing ancestor")?;
    }
    let mut resolved = canonicalize_existing(existing)?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn expand_home(raw: &str, home: &Path) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if raw == "~" => home.to_path_buf(),
        None => PathBuf::from(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str, workspace: &Path, home: &Path) -> Result<Policy> {
        resolve(
            serde_json::from_str(text)?,
            workspace,
            workspace,
            workspace,
            home,
        )
    }

    #[test]
    fn applies_matching_workspace_rules_and_expands_home() {
        let policy = parse(
            r#"{
                "sandbox": {
                    "network": {"allow_domains": ["crates.io"]},
                    "filesystem": {"read": ["~/.cargo/registry"]},
                    "workspaces": {
                        "/work/app": {"network": {"allow_domains": ["*.npmjs.org"]}},
                        "/work/other": {"network": {"allow_domains": ["example.com"]}}
                    }
                }
            }"#,
            Path::new("/work/app"),
            Path::new("/home/me"),
        )
        .unwrap();
        assert_eq!(policy.network, Network::Allowlist);
        assert_eq!(policy.allow_hosts, ["*.npmjs.org"]);
        assert_eq!(
            policy.read_paths,
            [PathBuf::from("/home/me/.cargo/registry")]
        );
    }

    #[test]
    fn rejects_bad_patterns_relative_paths_and_typos() {
        let home = Path::new("/home/me");
        let workspace = Path::new("/work");
        assert!(
            parse(
                r#"{"sandbox":{"network":{"allow_domains":["*"]}}}"#,
                workspace,
                home
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"sandbox":{"network":{"allow_domains":["foo*bar.com"]}}}"#,
                workspace,
                home
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"sandbox":{"filesystem":{"read":["relative/dir"]}}}"#,
                workspace,
                home
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"sandbox":{"network":{"allow_domain":["crates.io"]}}}"#,
                workspace,
                home
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"sandbox":{"network":{"mode":"allowlist"}}}"#,
                workspace,
                home
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"sandbox":{"network":{"mode":"open","allow_domains":["a.com"]}}}"#,
                workspace,
                home
            )
            .is_err()
        );
        assert!(
            parse(
                r#"{"sandbox":{"network":{"mode":"offline"}}}"#,
                workspace,
                home
            )
            .is_err()
        );
    }

    #[test]
    fn tool_timeout_defaults_validates_and_can_be_overridden_per_workspace() {
        let home = Path::new("/home/me");
        let work = Path::new("/work");
        assert_eq!(parse("{}", work, home).unwrap().tool_timeout_seconds, 120);
        let policy = parse(
            r#"{
                "tools": {"timeout_seconds": 600},
                "sandbox": {"workspaces": {"/work": {"tools": {"timeout_seconds": 900}}}}
            }"#,
            work,
            home,
        )
        .unwrap();
        assert_eq!(policy.tool_timeout_seconds, 900);
        assert!(parse(r#"{"tools":{"timeout_seconds":0}}"#, work, home).is_err());
        assert!(parse(r#"{"tools":{"timeout_seconds":3601}}"#, work, home).is_err());
    }

    #[test]
    fn network_is_blocked_by_default_and_workspaces_can_override_it() {
        let home = Path::new("/home/me");
        let policy = parse("{}", Path::new("/work"), home).unwrap();
        assert_eq!(policy.workspace, PathBuf::from("/work"));
        assert_eq!(policy.network, Network::Blocked);
        assert_eq!(policy.tool_timeout_seconds, 120);
        assert!(policy.allow_hosts.is_empty());
        assert!(policy.read_paths.is_empty());
        let policy = parse(
            r#"{
                "sandbox": {
                    "network": {"mode": "blocked"},
                    "workspaces": {"/work": {"network": {"mode": "open"}}}
                }
            }"#,
            Path::new("/work"),
            home,
        )
        .unwrap();
        assert_eq!(policy.network, Network::Open);
    }

    #[test]
    fn chooses_the_most_specific_containing_workspace() {
        let policy = resolve(
            serde_json::from_str(
                r#"{
                    "sandbox": {
                        "workspaces": {
                            "/work": {"tools": {"timeout_seconds": 200}},
                            "/work/nono": {"tools": {"timeout_seconds": 300}}
                        }
                    }
                }"#,
            )
            .unwrap(),
            Path::new("/work"),
            Path::new("/work/nono/src/main.rs"),
            Path::new("/work/nono/src"),
            Path::new("/home/me"),
        )
        .unwrap();
        assert_eq!(policy.workspace, PathBuf::from("/work/nono"));
        assert_eq!(policy.tool_timeout_seconds, 300);
    }

    #[test]
    fn configured_only_mode_denies_unmatched_operations() {
        let result = resolve(
            serde_json::from_str(
                r#"{
                    "sandbox": {
                        "unconfigured_workspaces": "deny",
                        "workspaces": {"/work/nono": {}}
                    }
                }"#,
            )
            .unwrap(),
            Path::new("/work"),
            Path::new("/work/other/file"),
            Path::new("/work/other"),
            Path::new("/home/me"),
        );
        assert!(result.is_err());
    }
}
