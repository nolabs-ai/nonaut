//! Sandbox settings from Nonaut's unified `config.json`.
//!
//! The file lives outside the workspace because sandboxed tools may write the
//! workspace and must not be able to widen their own permissions.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use nono::net_filter::validate_host_pattern;
use serde_json::json;
use sha2::{Digest, Sha256};

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
    pub write_paths: Vec<PathBuf>,
    pub path_entries: Vec<PathBuf>,
    pub command_home: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub profiles: Vec<String>,
}

pub(crate) struct AuditContext {
    pub workspace: PathBuf,
    pub profiles: Vec<String>,
    pub network_mode: String,
    pub policy_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationAccess {
    Read,
    Write,
}

pub(crate) const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 120;
const MAX_TOOL_TIMEOUT_SECONDS: u64 = 3600;

pub(crate) fn load(
    session_root: &Path,
    operation: &Path,
    fallback_root: &Path,
    access: OperationAccess,
) -> Result<Policy> {
    let config = crate::config::load()?;
    let home = BaseDirs::new()
        .context("could not determine the home directory")?
        .home_dir()
        .to_path_buf();
    let policy = resolve(
        config,
        session_root,
        operation,
        fallback_root,
        access,
        &home,
    )
    .with_context(|| {
        crate::config::config_path().map_or_else(
            |_| "invalid Nonaut configuration".to_owned(),
            |path| format!("invalid {}", path.display()),
        )
    })?;
    crate::runtime_paths::prepare_private_directory(&policy.command_home)
        .context("could not prepare the private command home")?;
    Ok(policy)
}

#[cfg(feature = "control-plane")]
pub(crate) fn audit_context(session_root: &Path) -> Result<AuditContext> {
    let policy = load(
        session_root,
        session_root,
        session_root,
        OperationAccess::Write,
    )?;
    Ok(policy.audit_context())
}

fn resolve(
    config: Config,
    session_root: &Path,
    operation: &Path,
    fallback_root: &Path,
    access: OperationAccess,
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

    let global_read_paths = resolve_read_paths(&filesystem, home)?;
    let global_write_paths = resolve_write_paths(&filesystem, home)?;

    let mut matching = None;
    for (configured, settings) in &config.sandbox.workspaces {
        let configured = expand_home(&configured.to_string_lossy(), home);
        if !configured.is_absolute() {
            bail!(
                "sandbox.workspaces key `{}` must be absolute or start with ~/",
                configured.display()
            );
        }
        let configured = canonicalize_nearest(&configured).with_context(|| {
            format!(
                "could not resolve configured workspace {}",
                configured.display()
            )
        })?;
        if operation.starts_with(&configured)
            && matching.as_ref().is_none_or(|(current, _): &(PathBuf, _)| {
                configured.components().count() > current.components().count()
            })
        {
            matching = Some((configured, settings));
        }
    }

    let (workspace, settings) = if let Some((path, settings)) = matching {
        (path, Some(settings))
    } else {
        let explicitly_readable = access == OperationAccess::Read
            && global_read_paths
                .iter()
                .any(|path| operation.starts_with(path));
        let explicitly_writable = access == OperationAccess::Write
            && global_write_paths
                .iter()
                .any(|path| operation.starts_with(path));
        if explicitly_readable || explicitly_writable {
            (session_root.clone(), None)
        } else if config.sandbox.unconfigured_workspaces == UnconfiguredWorkspaces::Deny {
            bail!(
                "operation at {} is outside every configured workspace",
                operation.display()
            );
        } else if !operation.starts_with(&session_root) {
            bail!(
                "operation at {} is outside the session scope {}",
                operation.display(),
                session_root.display()
            );
        } else {
            (fallback_root, None)
        }
    };

    let mut profiles = &[][..];
    if let Some(extra) = settings {
        profiles = &extra.profiles;
        if let Some(extra_network) = &extra.network {
            network = extra_network.clone();
        }
        if let Some(extra_filesystem) = &extra.filesystem {
            filesystem
                .read
                .extend(extra_filesystem.read.iter().cloned());
            filesystem
                .write
                .extend(extra_filesystem.write.iter().cloned());
        }
        if let Some(timeout) = extra
            .tools
            .as_ref()
            .and_then(|settings| settings.timeout_seconds)
        {
            tools.timeout_seconds = Some(timeout);
        }
    }

    validate(workspace, network, filesystem, tools, profiles, home)
}

fn validate(
    workspace: PathBuf,
    network: NetworkConfig,
    filesystem: FilesystemConfig,
    tools: WorkspaceToolsConfig,
    profiles: &[String],
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
    let mut read_paths = resolve_read_paths(&filesystem, home)?;
    let mut write_paths = resolve_write_paths(&filesystem, home)?;
    let command_home = crate::runtime_paths::workspace_cache(home, &["homes"], &workspace)?;
    write_paths.push(command_home.clone());
    let profile = crate::profiles::resolve(profiles, home, &workspace)?;
    read_paths.extend(profile.read_paths);
    write_paths.extend(profile.write_paths);
    Ok(Policy {
        command_home,
        workspace,
        network: network_mode,
        tool_timeout_seconds,
        allow_hosts: network.allow_domains,
        read_paths,
        write_paths,
        path_entries: profile.path_entries,
        environment: profile.environment,
        profiles: profiles.to_vec(),
    })
}

impl Policy {
    pub(crate) fn audit_context(&self) -> AuditContext {
        let network_mode = match self.network {
            Network::Open => "open",
            Network::Blocked => "blocked",
            Network::Allowlist => "allowlist",
        }
        .to_owned();
        let document = json!({
            "workspace": self.workspace,
            "network": network_mode,
            "allow_hosts": self.allow_hosts,
            "timeout_seconds": self.tool_timeout_seconds,
            "profiles": self.profiles,
            "read_paths": self.read_paths,
            "write_paths": self.write_paths,
            "path_entries": self.path_entries,
            "environment_names": self.environment.keys().collect::<Vec<_>>()
        });
        let digest = Sha256::digest(serde_json::to_vec(&document).unwrap_or_default());
        AuditContext {
            workspace: self.workspace.clone(),
            profiles: self.profiles.clone(),
            network_mode,
            policy_digest: format!("sha256:{}", hex(&digest)),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn resolve_write_paths(filesystem: &FilesystemConfig, home: &Path) -> Result<Vec<PathBuf>> {
    resolve_paths(&filesystem.write, home, "sandbox.filesystem.write")
}

fn resolve_read_paths(filesystem: &FilesystemConfig, home: &Path) -> Result<Vec<PathBuf>> {
    resolve_paths(&filesystem.read, home, "sandbox.filesystem.read")
}

fn resolve_paths(paths: &[String], home: &Path, setting: &str) -> Result<Vec<PathBuf>> {
    paths
        .iter()
        .map(|raw| {
            let path = expand_home(raw, home);
            if path.is_absolute() {
                Ok(path)
            } else {
                bail!("{setting} entry `{raw}` must be absolute or start with ~/")
            }
        })
        .collect()
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
            OperationAccess::Write,
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
        assert_ne!(policy.command_home, policy.workspace);
        assert!(policy.write_paths.contains(&policy.command_home));
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
            OperationAccess::Write,
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
            OperationAccess::Write,
            Path::new("/home/me"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn explicit_external_reads_work_without_granting_external_writes() {
        let config: Config = serde_json::from_str(
            r#"{
                "sandbox": {
                    "unconfigured_workspaces": "deny",
                    "filesystem": {"read": ["/outside"]}
                }
            }"#,
        )
        .unwrap();
        let read = resolve(
            config.clone(),
            Path::new("/work"),
            Path::new("/outside/file"),
            Path::new("/outside"),
            OperationAccess::Read,
            Path::new("/home/me"),
        )
        .unwrap();
        assert_eq!(read.workspace, PathBuf::from("/work"));
        assert_eq!(read.read_paths, [PathBuf::from("/outside")]);
        assert!(
            resolve(
                config,
                Path::new("/work"),
                Path::new("/outside/file"),
                Path::new("/outside"),
                OperationAccess::Write,
                Path::new("/home/me"),
            )
            .is_err()
        );

        let writable: Config = serde_json::from_str(
            r#"{
                "sandbox": {
                    "unconfigured_workspaces": "deny",
                    "filesystem": {"write": ["/outside"]}
                }
            }"#,
        )
        .unwrap();
        let write = resolve(
            writable,
            Path::new("/work"),
            Path::new("/outside/file"),
            Path::new("/outside"),
            OperationAccess::Write,
            Path::new("/home/me"),
        )
        .unwrap();
        assert!(write.write_paths.contains(&PathBuf::from("/outside")));
        assert!(write.write_paths.contains(&write.command_home));
    }

    #[test]
    fn relative_workspace_keys_are_rejected() {
        let result = resolve(
            serde_json::from_str(r#"{"sandbox":{"workspaces":{"relative/path":{}}}}"#).unwrap(),
            Path::new("/work"),
            Path::new("/work/file"),
            Path::new("/work"),
            OperationAccess::Read,
            Path::new("/home/me"),
        );
        assert!(result.unwrap_err().to_string().contains("must be absolute"));
    }
}
