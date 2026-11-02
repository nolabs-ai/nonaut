//! Resolved project policy, pinned for the lifetime of a session.

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
    Config, FilesystemConfig, NetworkConfig, ReferenceAccess, WorkspaceToolsConfig,
};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Policy {
    /// Canonical project identity; access is determined by explicit grants.
    pub workspace: PathBuf,
    pub name: String,
    pub source: PathBuf,
    pub sources: Vec<PathBuf>,
    pub references: BTreeMap<String, (PathBuf, ReferenceAccess)>,
    pub deny_paths: Vec<PathBuf>,
    pub protected_paths: Vec<PathBuf>,
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
    let loaded = crate::workspace::load(session_root)?;
    let mut policy = resolve(&config, loaded, session_root, &home).with_context(|| {
        crate::config::config_path().map_or_else(
            |_| "invalid Nonaut configuration".to_owned(),
            |path| format!("invalid {}", path.display()),
        )
    })?;
    crate::runtime_paths::prepare_private_directory(&policy.command_home)
        .context("could not prepare the private command home")?;
    crate::runtime_paths::prepare_private_directory(&policy.command_home.join("tmp"))?;
    prepare_capability_file(&mut policy, &home)?;
    Ok(policy)
}

pub(crate) fn describe(session_root: &Path) -> Result<String> {
    let policy = pin(session_root)?;
    describe_resolved(&policy, false)
}

pub(crate) fn describe_full(session_root: &Path) -> Result<String> {
    describe_resolved(pin(session_root)?.as_ref(), true)
}

fn describe_resolved(policy: &Policy, full: bool) -> Result<String> {
    let mut lines = vec![
        format!(
            "Workspace  {} · {}",
            policy.name,
            crate::workspace::display_path(&policy.workspace)
        ),
        format!("Policy     {}", policy.source.display()),
        String::new(),
        "FILESYSTEM".into(),
    ];
    for (access, paths) in [
        ("read", &policy.operation_read_paths),
        ("read/write", &policy.operation_write_paths),
        ("deny", &policy.deny_paths),
    ] {
        for path in paths {
            lines.push(format!(
                "  {:<44} {access}",
                crate::workspace::display_path(path)
            ));
        }
    }
    lines.push(format!(
        "Packs      {}",
        if policy.profiles.is_empty() {
            "none".into()
        } else {
            policy.profiles.join(", ")
        }
    ));
    for (name, (path, access)) in &policy.references {
        let access = match access {
            ReferenceAccess::Read => "read",
            ReferenceAccess::ReadWrite => "read/write",
        };
        lines.push(format!(
            "Reference  {name} · {} · {access}",
            crate::workspace::display_path(path)
        ));
    }
    lines.push(format!("Network    {:?}", policy.network));
    for host in &policy.allow_hosts {
        lines.push(format!("  {host}"));
    }
    lines.push(format!(
        "HOME       {} (isolated)",
        crate::workspace::display_path(&policy.command_home)
    ));
    lines.push(format!(
        "Sources    {}",
        policy
            .sources
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if full {
        lines.push("\nNONO RUNTIME".into());
        for (access, paths) in [
            ("read", &policy.read_paths),
            ("read/write", &policy.write_paths),
        ] {
            for path in paths {
                lines.push(format!("  {}  {access}", path.display()));
            }
        }
        for (access, paths) in [
            ("read", crate::sandbox::platform::read_paths()),
            ("write", crate::sandbox::platform::write_paths()),
        ] {
            for path in paths {
                if Path::new(path).exists() {
                    lines.push(format!("  {path}  {access} (system)"));
                }
            }
        }
        for (name, value) in &policy.environment {
            lines.push(format!("  {name}={value}"));
        }
        for path in &policy.protected_paths {
            lines.push(format!("  {}  protected from writes", path.display()));
        }
    } else {
        lines.push(
            "Runtime    isolated caches and platform support; /permissions full for all grants"
                .into(),
        );
    }
    Ok(lines.join("\n"))
}

#[cfg(feature = "control-plane")]
pub(crate) fn audit_context(session_root: &Path) -> Result<AuditContext> {
    let policy = pin(session_root)?;
    Ok(policy.audit_context())
}

pub(crate) fn validate_project(root: &Path, loaded: &crate::workspace::Loaded) -> Result<()> {
    resolve(
        &crate::config::load()?,
        loaded.clone(),
        root,
        &crate::workspace::home()?,
    )
    .map(|_| ())
}

fn resolve(
    config: &Config,
    loaded: crate::workspace::Loaded,
    session_root: &Path,
    home: &Path,
) -> Result<Policy> {
    let session_root = canonicalize_nearest(session_root)?;
    let project = &loaded.config;
    let mut filesystem = project.filesystem.clone();
    // Filesystem entries describe project paths; references name external grants.
    for path in filesystem.read.iter().chain(&filesystem.readwrite) {
        let resolved = resolve_project_path(path, &session_root, home)?;
        if !resolved.starts_with(&session_root) {
            bail!("filesystem path {path} leaves the project; use a named reference");
        }
    }
    let mut references = BTreeMap::new();
    for (name, reference) in &project.references {
        if name.trim().is_empty() {
            bail!("reference name cannot be empty");
        }
        let path = resolve_project_path(&reference.path, &session_root, home)?;
        references.insert(name.clone(), (path.clone(), reference.access));
        let raw = path.to_string_lossy().into_owned();
        match reference.access {
            ReferenceAccess::Read => filesystem.read.push(raw),
            ReferenceAccess::ReadWrite => filesystem.readwrite.push(raw),
        }
    }
    for paths in [
        &mut filesystem.read,
        &mut filesystem.readwrite,
        &mut filesystem.deny,
    ] {
        for raw in paths.iter_mut() {
            *raw = resolve_project_path(raw, &session_root, home)?
                .to_string_lossy()
                .into_owned();
        }
    }
    let tools = project.tools.clone().unwrap_or(WorkspaceToolsConfig {
        timeout_seconds: Some(config.tools.timeout_seconds),
    });
    let mut policy = validate(
        session_root,
        project.network.clone().unwrap_or_default(),
        filesystem,
        tools,
        &project.packs,
        home,
    )?;
    policy.name = project.name.clone().unwrap_or_else(|| {
        policy
            .workspace
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    policy.source = loaded.source;
    policy.sources = loaded.sources;
    policy.references = references;
    policy.protected_paths = vec![canonicalize_nearest(&crate::config::config_dir()?)?];
    policy.protected_paths.extend(policy.sources.clone());
    for protected in &policy.protected_paths {
        if (protected == &policy.protected_paths[0] || cfg!(target_os = "linux"))
            && policy
                .write_paths
                .iter()
                .any(|grant| protected.starts_with(grant) || grant.starts_with(protected))
        {
            bail!(
                "write grant overlaps protected Nonaut configuration {}",
                protected.display()
            );
        }
    }
    validate_deny_overlaps(&policy, cfg!(target_os = "linux"))?;
    Ok(policy)
}

fn resolve_project_path(raw: &str, root: &Path, home: &Path) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        bail!("path cannot be empty");
    }
    if raw.contains('*') || raw.contains('?') {
        bail!("filesystem paths must be literal paths, not glob patterns: {raw}");
    }
    let path = expand_home(raw, home);
    canonicalize_nearest(&if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn validate_deny_overlaps(policy: &Policy, linux: bool) -> Result<()> {
    if !linux {
        return Ok(());
    }
    let runtime = crate::sandbox::platform::read_paths()
        .iter()
        .chain(crate::sandbox::platform::write_paths())
        .map(PathBuf::from)
        .chain(std::env::split_paths(
            &crate::sandbox::platform::command_path(),
        ))
        // Match the filesystem grants installed by sandbox::allow_existing.
        // Linux descriptor links to pipes or sockets have no canonical path.
        .filter_map(|path| fs::canonicalize(path).ok())
        .collect::<Vec<_>>();
    for denied in &policy.deny_paths {
        for grant in policy
            .read_paths
            .iter()
            .chain(&policy.write_paths)
            .chain(&policy.path_entries)
            .chain(&runtime)
        {
            let grant = canonicalize_nearest(grant)?;
            if denied.starts_with(&grant) {
                bail!(
                    "Linux Landlock cannot enforce deny {} inside allowed {}; use narrower grants",
                    denied.display(),
                    grant.display()
                );
            }
        }
    }
    for protected in &policy.protected_paths {
        for grant in crate::sandbox::platform::write_paths() {
            if let Ok(grant) = fs::canonicalize(grant)
                && (protected.starts_with(&grant) || grant.starts_with(protected))
            {
                bail!(
                    "Linux runtime write grant {} overlaps protected configuration {}; place configuration outside that directory",
                    grant.display(),
                    protected.display()
                );
            }
        }
    }
    Ok(())
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
    let network_mode = network.mode;
    let tool_timeout_seconds = tools
        .timeout_seconds
        .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECONDS);
    if !(1..=MAX_TOOL_TIMEOUT_SECONDS).contains(&tool_timeout_seconds) {
        bail!("tools.timeout_seconds must be between 1 and {MAX_TOOL_TIMEOUT_SECONDS}");
    }
    let mut read_paths = resolve_read_paths(&filesystem, home)?;
    let deny_paths = resolve_paths(&filesystem.deny, home, "filesystem.deny")?;
    let mut write_paths = resolve_write_paths(&filesystem, home)?;
    for path in read_paths.iter().chain(&write_paths) {
        if !path.exists() {
            bail!(
                "granted path {} does not exist; create it before granting access",
                path.display()
            );
        }
    }
    read_paths.extend(write_paths.iter().cloned());
    let operation_read_paths = read_paths.clone();
    let operation_write_paths = write_paths.clone();
    let command_home = crate::runtime_paths::workspace_cache(home, &["homes"], &workspace)?;
    write_paths.push(command_home.clone());
    let profile = crate::profiles::resolve(profiles, home, &workspace)?;
    read_paths.extend(profile.read_paths);
    write_paths.extend(profile.write_paths);
    Ok(Policy {
        command_home,
        name: String::new(),
        source: PathBuf::new(),
        sources: Vec::new(),
        references: BTreeMap::new(),
        deny_paths,
        protected_paths: Vec::new(),
        workspace,
        network: network_mode,
        tool_timeout_seconds,
        allow_hosts: network.domains,
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
    match (network.mode, network.domains.is_empty()) {
        (Network::Allowlist, true) => {
            bail!("network.mode = allowlist needs at least one domains entry")
        }
        (Network::Open | Network::Blocked, false) => {
            bail!("domains only applies with network.mode = allowlist")
        }
        _ => {}
    }
    for host in &network.domains {
        if host == "*" {
            bail!("domains may not contain \"*\"; list the domains the sandbox needs");
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
        if self
            .deny_paths
            .iter()
            .any(|path| operation.starts_with(path))
            || (access == OperationAccess::Write
                && self
                    .protected_paths
                    .iter()
                    .any(|path| operation.starts_with(path)))
        {
            bail!("policy denies {:?} at {}", access, operation.display());
        }
        let allowed = (access == OperationAccess::Read
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
        let mut model_read = Vec::new();
        model_read.extend(self.operation_read_paths.iter().cloned());
        let mut model_write = Vec::new();
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
            "source": self.source,
            "sources": self.sources,
            "references": self.references,
            "deny": self.deny_paths,
            "protected_write": self.protected_paths,
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
            "packs": self.profiles,
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
    resolve_paths(&filesystem.readwrite, home, "filesystem.readwrite")
}

fn resolve_read_paths(filesystem: &FilesystemConfig, home: &Path) -> Result<Vec<PathBuf>> {
    resolve_paths(&filesystem.read, home, "filesystem.read")
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

pub(crate) fn canonicalize_nearest(path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    if !path.is_absolute() {
        bail!("expected an absolute path: {}", path.display());
    }
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            component => {
                resolved.push(component.as_os_str());
                match fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        resolved = fs::canonicalize(&resolved)
                            .with_context(|| format!("could not resolve {}", resolved.display()))?
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("could not inspect {}", resolved.display()));
                    }
                }
            }
        }
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
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn fixture(value: serde_json::Value) -> (PathBuf, Policy) {
        let root = std::env::temp_dir().join(format!(
            "nonaut-policy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let policy = resolve(
            &Config::default(),
            crate::workspace::Loaded {
                source: root.with_extension("json"),
                sources: Vec::new(),
                config: serde_json::from_value(value).unwrap(),
            },
            &root,
            &root,
        )
        .unwrap();
        (root, policy)
    }
    #[test]
    fn project_access_is_explicit_and_read_only_stays_read_only() {
        let (root, policy) = fixture(json!({"filesystem":{"read":["."]}}));
        assert!(
            policy
                .authorize(&root.join("file"), &root, OperationAccess::Read)
                .is_ok()
        );
        assert!(
            policy
                .authorize(&root.join("file"), &root, OperationAccess::Write)
                .is_err()
        );
        assert!(
            policy
                .authorize(
                    &root.parent().unwrap().join("outside"),
                    &root,
                    OperationAccess::Read
                )
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn empty_policy_grants_no_project_access() {
        let (root, policy) = fixture(json!({}));
        assert!(
            policy
                .authorize(&root, &root, OperationAccess::Read)
                .is_err()
        );
        assert!(
            policy
                .authorize(&root, &root, OperationAccess::Write)
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_descriptor_grants_support_pipes_but_check_real_files() {
        use std::process::Command;

        let child = || {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "policy::tests::empty_policy_grants_no_project_access",
                "--nocapture",
            ]);
            command
        };
        let output = child().output().unwrap();
        assert!(
            output.status.success(),
            "policy validation failed with piped stdout/stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let (root, _) = fixture(json!({}));
        let config = root.join("nonaut");
        fs::create_dir_all(&config).unwrap();
        let output = child()
            .env("XDG_CONFIG_HOME", &root)
            .stdout(fs::File::create(config.join("output")).unwrap())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Linux runtime write grant"),
            "a stdout file inside protected configuration must be rejected: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn reference_files_are_supported_without_inheriting_other_policies() {
        let path = std::env::current_exe().unwrap();
        let (root, policy) =
            fixture(json!({"references":{"executable":{"path":path,"access":"read"}}}));
        assert!(
            policy
                .authorize(&path, &root, OperationAccess::Read)
                .is_ok()
        );
        assert!(
            policy
                .authorize(&path, &root, OperationAccess::Write)
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn denies_override_all_model_grants_and_linux_conflicts_fail() {
        let (root, mut policy) = fixture(json!({"filesystem":{"readwrite":["."]}}));
        policy.deny_paths.push(root.join("secret.env"));
        assert!(
            policy
                .authorize(&root.join("secret.env"), &root, OperationAccess::Read)
                .is_err()
        );
        assert!(
            policy
                .authorize(&root.join("secret.env"), &root, OperationAccess::Write)
                .is_err()
        );
        assert!(validate_deny_overlaps(&policy, true).is_err());
        assert!(validate_deny_overlaps(&policy, false).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn canonicalization_resolves_missing_paths_and_parent_components() {
        let root = std::env::current_dir().unwrap();
        assert_eq!(
            resolve_project_path("./missing/../secret", &root, &root).unwrap(),
            root.join("secret")
        );
        let missing = root.join(format!("nonaut-missing-grant-{}", std::process::id()));
        assert!(
            validate(
                root.clone(),
                NetworkConfig::default(),
                FilesystemConfig {
                    read: vec![missing.to_string_lossy().into_owned()],
                    ..Default::default()
                },
                WorkspaceToolsConfig::default(),
                &[],
                &root
            )
            .is_err()
        );
    }
    #[test]
    fn invalid_network_and_timeout_are_rejected() {
        for value in [
            json!({"mode":"allowlist"}),
            json!({"mode":"open","domains":["example.com"]}),
            json!({"mode":"allowlist","domains":["*"]}),
        ] {
            let network: NetworkConfig = serde_json::from_value(value).unwrap();
            assert!(validate_network_config(&network).is_err());
        }
        let root = std::env::current_dir().unwrap();
        for timeout in [0, MAX_TOOL_TIMEOUT_SECONDS + 1] {
            assert!(
                validate(
                    root.clone(),
                    NetworkConfig::default(),
                    FilesystemConfig::default(),
                    WorkspaceToolsConfig {
                        timeout_seconds: Some(timeout)
                    },
                    &[],
                    &root
                )
                .is_err()
            );
        }
    }
    #[test]
    fn manifest_and_summary_report_the_pinned_policy() {
        let (root, policy) = fixture(json!({"filesystem":{"readwrite":["."]}}));
        let manifest = policy.capability_manifest();
        assert_eq!(
            manifest["model_access"]["write"][0],
            root.to_string_lossy().as_ref()
        );
        assert!(
            manifest["policy_digest"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        let summary = describe_resolved(&policy, false).unwrap();
        assert!(summary.contains("/permissions full"));
        assert!(!summary.contains("NONO RUNTIME"));
        assert!(
            describe_resolved(&policy, true)
                .unwrap()
                .contains("NONO RUNTIME")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
