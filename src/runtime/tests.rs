use super::*;
use std::{
    fs,
    os::unix::fs::symlink,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    host: HostInventory,
    command_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nonaut-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("workspace")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let home = root.join("home");
        let mut host = HostInventory {
            platform: inventory::Platform::current(),
            home: home.clone(),
            workspace: root.join("workspace"),
            executable_paths: Vec::new(),
            homebrew: Vec::new(),
            developer_directories: Vec::new(),
            locations: BTreeMap::new(),
        };
        for (name, child) in [
            ("CARGO_HOME", ".cargo"),
            ("RUSTUP_HOME", ".rustup"),
            ("PYENV_ROOT", ".pyenv"),
            ("NVM_DIR", ".nvm"),
            ("FNM_DIR", ".fnm"),
            ("BUN_INSTALL", ".bun"),
            ("GOROOT", "go"),
        ] {
            host.locations.insert(name.into(), home.join(child));
        }
        let command_home = root.join("private");
        Self {
            root,
            host,
            command_home,
        }
    }

    fn resolve(&self, value: serde_json::Value) -> Result<Plan> {
        resolve_with_host(
            &serde_json::from_value(value)?,
            &self.host,
            &self.command_home,
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn custom_has_only_explicit_and_executor_grants_and_no_implicit_environment() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.host.home.join(".cargo/bin")).unwrap();
    let plan = fixture.resolve(serde_json::json!({"mode":"custom", "filesystem":{"read":["../workspace"]}, "environment":{"PATH":"/opt/company/bin", "COMPANY_OPTION":"secret-value"}})).unwrap();
    assert!(plan.packs.is_empty());
    assert_eq!(plan.grants.len(), 2);
    assert!(
        plan.grants
            .iter()
            .all(|grant| matches!(grant.kind, GrantKind::Explicit | GrantKind::Executor))
    );
    assert_eq!(plan.environment.len(), 2);
    assert_eq!(plan.path_entries, [PathBuf::from("/opt/company/bin")]);
    assert!(
        !serde_json::to_string(&plan)
            .unwrap()
            .contains("secret-value")
    );
    assert!(
        !fixture.command_home.exists(),
        "resolution must have no filesystem side effects"
    );
}

#[test]
fn manual_selection_has_no_platform_fallback_and_auto_exclusions_remove_all_pack_effects() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.host.home.join(".cargo/bin")).unwrap();
    fs::create_dir_all(fixture.host.home.join(".rustup")).unwrap();
    fs::write(fixture.host.workspace.join("Cargo.toml"), "[package]").unwrap();
    let manual = fixture
        .resolve(serde_json::json!({"mode":"manual","packs":["rust"]}))
        .unwrap();
    assert_eq!(
        manual
            .packs
            .iter()
            .map(|pack| pack.name.as_str())
            .collect::<Vec<_>>(),
        ["rust"]
    );
    assert!(
        manual
            .grants
            .iter()
            .all(|grant| matches!(grant.source.as_str(), "rust" | "executor"))
    );
    assert!(manual.environment.contains_key("CARGO_HOME"));
    let auto = fixture
        .resolve(serde_json::json!({"mode":"auto","exclude":["rust"]}))
        .unwrap();
    assert!(auto.packs.iter().any(|pack| pack.name == "system"));
    assert!(!auto.packs.iter().any(|pack| pack.name == "rust"));
    assert!(!auto.environment.contains_key("CARGO_HOME"));
    assert!(!auto.grants.iter().any(|grant| grant.source == "rust"));
    assert!(
        !auto
            .path_entries
            .iter()
            .any(|path| path.starts_with(fixture.host.home.join(".cargo")))
    );
}

#[test]
fn project_detection_combines_markers_without_executing_project_code() {
    let fixture = Fixture::new();
    for marker in [
        "Cargo.toml",
        "package.json",
        "requirements.txt",
        "go.mod",
        "bun.lock",
    ] {
        fs::write(
            fixture.host.workspace.join(marker),
            "not executable or parsed",
        )
        .unwrap();
    }
    assert_eq!(
        integrations::detect_project(&fixture.host.workspace),
        ["rust", "python", "node", "go", "bun"]
    );
    let plan = fixture.resolve(serde_json::json!({"mode":"auto"})).unwrap();
    assert_eq!(
        plan.packs
            .iter()
            .filter(|pack| pack.reason == "project marker")
            .count(),
        5
    );
}

#[test]
fn default_runtime_detects_project_and_platform_support() {
    let fixture = Fixture::new();
    fs::write(fixture.host.workspace.join("Cargo.toml"), "[package]").unwrap();
    let plan = resolve_with_host(
        &RuntimeConfig::default(),
        &fixture.host,
        &fixture.command_home,
    )
    .unwrap();
    assert_eq!(plan.mode, Mode::Auto);
    assert!(plan.packs.iter().any(|pack| pack.name == "system"));
    assert!(
        plan.packs
            .iter()
            .any(|pack| pack.name == "rust" && pack.reason == "project marker")
    );
}

#[test]
fn unknown_duplicate_and_conflicting_selections_fail_closed() {
    let fixture = Fixture::new();
    for value in [
        serde_json::json!({"packs":["unknown"]}),
        serde_json::json!({"packs":["rust","rust"]}),
        serde_json::json!({"mode":"custom","packs":["system"]}),
        serde_json::json!({"mode":"manual","exclude":["system"]}),
    ] {
        assert!(fixture.resolve(value).is_err());
    }
    let names = integrations::CATALOG
        .iter()
        .map(|pack| pack.name)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), integrations::CATALOG.len());
}

#[test]
fn internal_environment_and_relative_path_entries_are_rejected() {
    let fixture = Fixture::new();
    for name in [
        "HOME",
        "TMPDIR",
        "NONAUT_CAP_FILE",
        "nonaut_proxy_port",
        "HTTPS_PROXY",
        "no_proxy",
        "1INVALID",
        "A=B",
    ] {
        let value = serde_json::json!({"mode":"custom","environment":{name:"overridden"}});
        assert!(fixture.resolve(value).is_err(), "{name}");
    }
    assert!(
        fixture
            .resolve(serde_json::json!({"environment":{"PATH":"/usr/bin:.:/bin"}}))
            .is_err()
    );
    assert!(
        fixture
            .resolve(serde_json::json!({"filesystem":{"read":["/missing/*"]}}))
            .is_err()
    );
}

#[test]
fn imported_explicit_mode_replaces_environment_paths_and_selection_but_retains_denials() {
    let mut base: RuntimeConfig = serde_json::from_value(serde_json::json!({"mode":"manual","packs":["rust"],"filesystem":{"read":["/opt/old"],"deny":["/secret"]},"environment":{"OLD":"value"}})).unwrap();
    base.merge(
        serde_json::from_value(serde_json::json!({"mode":"custom","environment":{"NEW":"value"}}))
            .unwrap(),
    );
    assert_eq!(base.mode, Some(Mode::Custom));
    assert!(base.packs.is_empty());
    assert!(base.filesystem.read.is_empty());
    assert_eq!(base.filesystem.deny, ["/secret"]);
    assert!(!base.environment.contains_key("OLD"));
    base.merge(
        serde_json::from_value(
            serde_json::json!({"filesystem":{"read":["/opt/new"]},"environment":{"EXTRA":"value"}}),
        )
        .unwrap(),
    );
    assert_eq!(base.filesystem.read, ["/opt/new"]);
    assert_eq!(base.environment.len(), 2);
}

#[test]
fn untrusted_or_writable_tool_installations_cannot_be_automatically_granted() {
    let fixture = Fixture::new();
    let cargo = fixture.host.home.join(".cargo/bin");
    fs::create_dir_all(&cargo).unwrap();
    assert!(fixture.host.trusted_installation(&cargo));
    assert!(!fixture.host.trusted_installation(&fixture.host.home));
    assert!(!fixture.host.trusted_installation(&fixture.host.workspace));
    let plan = fixture
        .resolve(serde_json::json!({"mode":"manual","packs":["rust"]}))
        .unwrap();
    assert!(
        plan.validate_writes(std::slice::from_ref(&fixture.host.home))
            .is_err()
    );
    fs::remove_dir(&cargo).unwrap();
    symlink(&fixture.host.workspace, &cargo).unwrap();
    assert!(
        fixture
            .resolve(serde_json::json!({"mode":"manual","packs":["rust"]}))
            .is_err()
    );
}

#[test]
fn custom_rust_installations_resolve_without_default_home_paths() {
    let mut fixture = Fixture::new();
    let installation = fixture.root.join("company-rust");
    fs::create_dir_all(installation.join("cargo/bin")).unwrap();
    fs::create_dir_all(installation.join("rustup")).unwrap();
    fixture
        .host
        .locations
        .insert("CARGO_HOME".into(), installation.join("cargo"));
    fixture
        .host
        .locations
        .insert("RUSTUP_HOME".into(), installation.join("rustup"));
    let plan = fixture
        .resolve(serde_json::json!({"mode":"manual","packs":["rust"]}))
        .unwrap();
    assert_eq!(
        plan.environment["RUSTUP_HOME"],
        installation.join("rustup").to_str().unwrap()
    );
    assert!(
        plan.grants
            .iter()
            .any(|grant| grant.path == installation.join("cargo/bin"))
    );
    assert!(
        plan.private_directories
            .iter()
            .any(|path| path.ends_with("bundled-cli"))
    );
}

#[test]
fn private_directory_preparation_rejects_symlinks_without_changing_their_targets() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).unwrap();
    let original = fs::metadata(&outside).unwrap().permissions();
    symlink(&outside, &fixture.command_home).unwrap();
    assert!(crate::runtime_paths::prepare_private_directory(&fixture.command_home).is_err());
    assert_eq!(fs::metadata(&outside).unwrap().permissions(), original);
}
