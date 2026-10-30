//! Sandbox settings from Nonaut's unified `config.json`.
//!
//! The file lives outside the workspace because sandboxed tools may write the
//! workspace and must not be able to widen their own permissions.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use nono::net_filter::validate_host_pattern;
use serde_json::json;
use sha2::{Digest, Sha256};

pub(crate) use crate::config::NetworkMode as Network;
use crate::config::{
    Config, FilesystemConfig, NetworkConfig, WorkspaceConfig, WorkspaceToolsConfig,
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
    /// Paths the model may address directly, excluding toolchain internals.
    operation_read_paths: Vec<PathBuf>,
    operation_write_paths: Vec<PathBuf>,
    pub path_entries: Vec<PathBuf>,
    pub command_home: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub profiles: Vec<String>,
    /// Exact, read-only description of this pinned policy for sandboxed tools.
    pub capability_file: Option<PathBuf>,
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

static PINNED: OnceLock<Mutex<HashMap<PathBuf, Arc<Policy>>>> = OnceLock::new();

pub(crate) fn pin(session_root: &Path) -> Result<Arc<Policy>> {
    let key = canonicalize_nearest(session_root)?;
    let pinned = PINNED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pinned = pinned.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(policy) = pinned.get(&key) {
        return Ok(Arc::clone(policy));
    }
    let policy = Arc::new(load_session(&key)?);
    pinned.insert(key, Arc::clone(&policy));
    Ok(policy)
}

pub(crate) fn load_session(session_root: &Path) -> Result<Policy> {
    let config = crate::config::load()?;
    let home = BaseDirs::new()
        .context("could not determine the home directory")?
        .home_dir()
        .to_path_buf();
    let mut policy = resolve(config, session_root, &home).with_context(|| {
        crate::config::config_path().map_or_else(
            |_| "invalid Nonaut configuration".to_owned(),
            |path| format!("invalid {}", path.display()),
        )
    })?;
    crate::runtime_paths::prepare_private_directory(&policy.command_home)
        .context("could not prepare the private command home")?;
    prepare_capability_file(&mut policy, &home)?;
    Ok(policy)
}

#[cfg(feature = "control-plane")]
pub(crate) fn audit_context(session_root: &Path) -> Result<AuditContext> {
    let policy = pin(session_root)?;
    Ok(policy.audit_context())
}

fn resolve(config: Config, session_root: &Path, home: &Path) -> Result<Policy> {
    let session_root = canonicalize_nearest(session_root)?;
    let configured = crate::workspace::config_for_root(&config, &session_root)?;
    let mut network = config.sandbox.network;
    let mut filesystem = config.sandbox.filesystem;
    let mut tools = WorkspaceToolsConfig {
        timeout_seconds: Some(config.tools.timeout_seconds),
    };

    let settings = configured.as_ref().map(|(_, settings)| settings);
    let mut profiles = &[][..];
    let mut references = Vec::new();
    if let Some(extra) = settings {
        profiles = &extra.profiles;
        references = resolve_references(extra, &session_root)?;
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

    filesystem.read.extend(
        references
            .iter()
            .map(|path| path.to_string_lossy().into_owned()),
    );

    validate(session_root, network, filesystem, tools, profiles, home)
}

fn resolve_references(workspace: &WorkspaceConfig, root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for (name, reference) in &workspace.references {
        crate::workspace::validate_name(name)
            .with_context(|| format!("invalid reference name `{name}`"))?;
        let path = crate::workspace::reference_root(&reference.path)?;
        if path.starts_with(root) || root.starts_with(&path) {
            bail!(
                "reference `{name}` overlaps writable workspace {}",
                root.display()
            );
        }
        if paths.contains(&path) {
            bail!("reference `{name}` duplicates another reference path");
        }
        paths.push(path);
    }
    Ok(paths)
}

fn validate(
    workspace: PathBuf,
    network: NetworkConfig,
    filesystem: FilesystemConfig,
    tools: WorkspaceToolsConfig,
    profiles: &[String],
    home: &Path,
) -> Result<Policy> {
    validate_network_config(&network)?;
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
    let mut read_paths = resolve_read_paths(&filesystem, home)?;
    let mut write_paths = resolve_write_paths(&filesystem, home)?;
    let operation_read_paths = read_paths.clone();
    let operation_write_paths = write_paths.clone();
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
        operation_read_paths,
        operation_write_paths,
        path_entries: profile.path_entries,
        environment: profile.environment,
        profiles: profiles.to_vec(),
        capability_file: None,
    })
}

pub(crate) fn validate_network_config(network: &NetworkConfig) -> Result<()> {
    match (network.mode, network.allow_domains.is_empty()) {
        (Some(Network::Allowlist), true) => {
            bail!("sandbox.network.mode = allowlist needs at least one allow_domains entry")
        }
        (Some(Network::Open | Network::Blocked), false) => {
            bail!("allow_domains only applies with sandbox.network.mode = allowlist")
        }
        _ => {}
    }
    for host in &network.allow_domains {
        if host == "*" {
            bail!("allow_domains may not contain \"*\"; list the domains the sandbox needs");
        }
        validate_host_pattern(host).map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

impl Policy {
    pub(crate) fn authorize(
        &self,
        operation: &Path,
        fallback_root: &Path,
        access: OperationAccess,
    ) -> Result<()> {
        let operation = canonicalize_nearest(operation)?;
        let _fallback_root = canonicalize_nearest(fallback_root)?;
        let allowed = operation.starts_with(&self.workspace)
            || (access == OperationAccess::Read
                && self.capability_file.as_ref() == Some(&operation))
            || (access == OperationAccess::Read
                && self
                    .operation_read_paths
                    .iter()
                    .any(|path| operation.starts_with(path)))
            || (access == OperationAccess::Write
                && self
                    .operation_write_paths
                    .iter()
                    .any(|path| operation.starts_with(path)));
        if allowed {
            Ok(())
        } else {
            bail!(
                "operation at {} is outside workspace {}",
                operation.display(),
                self.workspace.display()
            )
        }
    }

    pub(crate) fn audit_context(&self) -> AuditContext {
        let network_mode = match self.network {
            Network::Open => "open",
            Network::Blocked => "blocked",
            Network::Allowlist => "allowlist",
        }
        .to_owned();
        AuditContext {
            workspace: self.workspace.clone(),
            profiles: self.profiles.clone(),
            network_mode,
            policy_digest: self.capability_manifest()["policy_digest"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        }
    }

    /// A sanitized description of the policy, never the source of authority.
    pub(crate) fn capability_manifest(&self) -> serde_json::Value {
        let network_mode = match self.network {
            Network::Open => "open",
            Network::Blocked => "blocked",
            Network::Allowlist => "allowlist",
        };
        let mut model_read = vec![self.workspace.clone()];
        model_read.extend(self.operation_read_paths.iter().cloned());
        let mut model_write = vec![self.workspace.clone()];
        model_write.extend(self.operation_write_paths.iter().cloned());
        deduplicate_paths(&mut model_read);
        deduplicate_paths(&mut model_write);

        let runtime_read = self
            .read_paths
            .iter()
            .filter(|path| self.capability_file.as_ref() != Some(path))
            .collect::<Vec<_>>();
        let executable_path_entries = self
            .path_entries
            .iter()
            .cloned()
            .chain(std::env::split_paths(
                &crate::sandbox::platform::command_path(),
            ))
            .collect::<Vec<_>>();
        let mut environment_names = ["HOME", "LANG", "NONAUT_CAP_FILE", "PATH", "TERM", "TMPDIR"]
            .into_iter()
            .map(str::to_owned)
            .chain(self.environment.keys().cloned())
            .collect::<BTreeSet<_>>();
        if self.network == Network::Allowlist {
            environment_names.extend(
                ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"]
                    .into_iter()
                    .map(str::to_owned),
            );
        }
        let document = json!({
            "schema_version": 1,
            "enforced_by": "nono",
            "workspace": self.workspace,
            "model_access": {
                "read": model_read,
                "write": model_write
            },
            "runtime_support": {
                "configured_read": runtime_read,
                "configured_write": self.write_paths,
                "system_read": crate::sandbox::platform::read_paths(),
                "system_write": crate::sandbox::platform::write_paths(),
                "executable_path_entries": executable_path_entries
            },
            "capability_file": {
                "path": self.capability_file,
                "environment": "NONAUT_CAP_FILE",
                "access": "read-only"
            },
            "network": {
                "mode": network_mode,
                "allow_hosts": self.allow_hosts
            },
            "profiles": self.profiles,
            "environment_names": environment_names,
            "tool_timeout_seconds": self.tool_timeout_seconds,
            "notes": [
                "This file describes the pinned policy; the operating-system sandbox is authoritative.",
                "Runtime-support paths enable tools but are not directly addressable by model file tools.",
                "Configuration changes require a new Nonaut session."
            ]
        });
        let digest = Sha256::digest(serde_json::to_vec(&document).unwrap_or_default());
        let mut manifest = document;
        manifest["policy_digest"] = json!(format!("sha256:{}", hex(&digest)));
        manifest
    }

    pub(crate) fn capability_manifest_json(&self) -> Result<String> {
        serde_json::to_string_pretty(&self.capability_manifest())
            .context("could not encode the capability manifest")
    }
}

fn deduplicate_paths(paths: &mut Vec<PathBuf>) {
    let mut unique = Vec::with_capacity(paths.len());
    paths.retain(|path| {
        if unique.contains(path) {
            false
        } else {
            unique.push(path.clone());
            true
        }
    });
}

fn prepare_capability_file(policy: &mut Policy, home: &Path) -> Result<()> {
    let directory =
        crate::runtime_paths::workspace_cache(home, &["capabilities"], &policy.workspace)?;
    crate::runtime_paths::prepare_private_directory(&directory)?;
    let path = directory.join(format!("{}.json", std::process::id()));
    policy.capability_file = Some(path.clone());
    let contents = policy.capability_manifest_json()?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("could not create {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("could not sync {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("could not protect {}", path.display()))?;
    policy.read_paths.push(path.clone());
    Ok(())
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
                canonicalize_nearest(&path)
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    fn roots(name: &str) -> (PathBuf, PathBuf) {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nonaut-policy-{name}-{}-{sequence}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        (root, workspace)
    }

    fn parse(text: &str, workspace: &Path, home: &Path) -> Result<Policy> {
        resolve(serde_json::from_str(text)?, workspace, home)
    }

    fn resolve_authorized(
        config: Config,
        workspace: &Path,
        operation: &Path,
        access: OperationAccess,
        home: &Path,
    ) -> Result<Policy> {
        let policy = resolve(config, workspace, home)?;
        policy.authorize(operation, operation, access)?;
        Ok(policy)
    }

    #[test]
    fn applies_matching_workspace_rules_and_expands_home() {
        let (root, workspace) = roots("matching");
        let config = serde_json::json!({
            "sandbox": {
                "network": {"allow_domains": ["crates.io"]},
                "filesystem": {"read": ["~/.cargo/registry"]}
            },
            "workspace": {"definitions": {
                "app": {"path": workspace, "network": {"allow_domains": ["*.npmjs.org"]}}
            }}
        });
        let policy = parse(&config.to_string(), &workspace, Path::new("/home/me")).unwrap();
        assert_eq!(policy.network, Network::Allowlist);
        assert_eq!(policy.allow_hosts, ["*.npmjs.org"]);
        assert_eq!(
            policy.read_paths,
            [canonicalize_nearest(Path::new("/home/me/.cargo/registry")).unwrap()]
        );
        fs::remove_dir_all(root).unwrap();
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
        let (root, work) = roots("timeout");
        let home = Path::new("/home/me");
        assert_eq!(parse("{}", &work, home).unwrap().tool_timeout_seconds, 120);
        let policy = parse(
            &format!(r#"{{
                "tools": {{"timeout_seconds": 600}},
                "workspace": {{"definitions": {{"test": {{"path": {}, "tools": {{"timeout_seconds": 900}}}}}}}}
            }}"#, serde_json::to_string(&work.to_string_lossy()).unwrap()),
            &work,
            home,
        )
        .unwrap();
        assert_eq!(policy.tool_timeout_seconds, 900);
        assert!(parse(r#"{"tools":{"timeout_seconds":0}}"#, &work, home).is_err());
        assert!(parse(r#"{"tools":{"timeout_seconds":3601}}"#, &work, home).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn network_is_blocked_by_default_and_workspaces_can_override_it() {
        let (root, work) = roots("network");
        let home = Path::new("/home/me");
        let policy = parse("{}", &work, home).unwrap();
        assert_eq!(policy.workspace, work.canonicalize().unwrap());
        assert_eq!(policy.network, Network::Blocked);
        assert_eq!(policy.tool_timeout_seconds, 120);
        assert!(policy.allow_hosts.is_empty());
        assert!(policy.read_paths.is_empty());
        assert_ne!(policy.command_home, policy.workspace);
        assert!(policy.write_paths.contains(&policy.command_home));
        let config = serde_json::json!({
            "sandbox": {"network": {"mode": "blocked"}},
            "workspace": {"definitions": {
                "test": {"path": work, "network": {"mode": "open"}}
            }}
        });
        let policy = parse(&config.to_string(), &work, home).unwrap();
        assert_eq!(policy.network, Network::Open);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_session_cannot_switch_to_another_configured_workspace() {
        let (root, first) = roots("pinned");
        let second = root.join("second");
        fs::create_dir_all(&second).unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "workspace": {"definitions": {
                "first": {"path": first},
                "second": {"path": second}
            }}
        }))
        .unwrap();
        let result = resolve_authorized(
            config,
            &first,
            &second.join("file"),
            OperationAccess::Read,
            Path::new("/home/me"),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("outside workspace")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn references_are_read_only() {
        let (root, workspace) = roots("reference");
        let reference = root.join("reference");
        fs::create_dir_all(&reference).unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "workspace": {"definitions": {"test": {
                "path": workspace,
                "references": {"nono": {"path": reference}}
            }}}
        }))
        .unwrap();
        let read = resolve_authorized(
            config.clone(),
            &workspace,
            &reference.join("src/lib.rs"),
            OperationAccess::Read,
            Path::new("/home/me"),
        )
        .unwrap();
        assert!(read.read_paths.contains(&reference.canonicalize().unwrap()));
        assert!(
            resolve_authorized(
                config,
                &workspace,
                &reference.join("src/lib.rs"),
                OperationAccess::Write,
                Path::new("/home/me"),
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_paths_are_capabilities_not_model_addressable_grants() {
        let (root, workspace) = roots("profile-scope");
        let home = root.join("home");
        let cargo_bin = home.join(".cargo/bin");
        let rustup = home.join(".rustup");
        fs::create_dir_all(&cargo_bin).unwrap();
        fs::create_dir_all(&rustup).unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "workspace": {"definitions": {"test": {
                "path": workspace,
                "profiles": ["rust"]
            }}}
        }))
        .unwrap();
        let policy = resolve(config, &workspace, &home).unwrap();
        assert!(policy.read_paths.contains(&cargo_bin));
        assert!(
            policy
                .authorize(&cargo_bin.join("cargo"), &cargo_bin, OperationAccess::Read)
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn capability_manifest_is_sanitized_and_matches_the_audit_digest() {
        let (root, workspace) = roots("manifest");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({
            "sandbox": {
                "network": {"allow_domains": ["crates.io"]},
                "filesystem": {"read": [outside]}
            }
        }))
        .unwrap();
        let mut policy = resolve(config, &workspace, &root).unwrap();
        policy
            .environment
            .insert("EXAMPLE_TOKEN".to_owned(), "never-disclose-this".to_owned());

        let manifest = policy.capability_manifest();
        let encoded = serde_json::to_string(&manifest).unwrap();
        assert_eq!(manifest["enforced_by"], "nono");
        assert_eq!(manifest["network"]["mode"], "allowlist");
        assert!(
            manifest["model_access"]["read"]
                .as_array()
                .unwrap()
                .contains(&json!(outside.canonicalize().unwrap()))
        );
        assert!(encoded.contains("EXAMPLE_TOKEN"));
        assert!(!encoded.contains("never-disclose-this"));
        assert_eq!(
            manifest["policy_digest"].as_str().unwrap(),
            policy.audit_context().policy_digest
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_external_reads_work_without_granting_external_writes() {
        let (root, work) = roots("external");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let config: Config = serde_json::from_str(
            &serde_json::json!({
                "sandbox": {"filesystem": {"read": [outside]}}
            })
            .to_string(),
        )
        .unwrap();
        let read = resolve_authorized(
            config.clone(),
            &work,
            &outside.join("file"),
            OperationAccess::Read,
            Path::new("/home/me"),
        )
        .unwrap();
        assert_eq!(read.workspace, work.canonicalize().unwrap());
        let outside = outside.canonicalize().unwrap();
        assert_eq!(read.read_paths, std::slice::from_ref(&outside));
        assert!(
            resolve_authorized(
                config,
                &work,
                &outside.join("file"),
                OperationAccess::Write,
                Path::new("/home/me"),
            )
            .is_err()
        );

        let writable: Config = serde_json::from_str(
            &serde_json::json!({
                "sandbox": {"filesystem": {"write": [outside]}}
            })
            .to_string(),
        )
        .unwrap();
        let write = resolve_authorized(
            writable,
            &work,
            &outside.join("file"),
            OperationAccess::Write,
            Path::new("/home/me"),
        )
        .unwrap();
        assert!(write.write_paths.contains(&outside));
        assert!(write.write_paths.contains(&write.command_home));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relative_workspace_paths_are_rejected() {
        let config: Config = serde_json::from_str(
            r#"{"workspace":{"definitions":{"test":{"path":"relative/path"}}}}"#,
        )
        .unwrap();
        let result = crate::workspace::workspace_root(&config.workspace.definitions["test"]);
        assert!(format!("{:#}", result.unwrap_err()).contains("must be absolute"));
    }
}
