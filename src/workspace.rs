use crate::config::{FilesystemConfig, NetworkConfig, ProjectConfig};
use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

pub(crate) const CONFIG_ENV: &str = "NONAUT_PROJECT_CONFIG";
static SELECTED: OnceLock<Mutex<BTreeMap<PathBuf, Loaded>>> = OnceLock::new();

#[derive(Clone, Debug)]
pub(crate) struct Loaded {
    pub source: PathBuf,
    pub sources: Vec<PathBuf>,
    pub config: ProjectConfig,
}

#[derive(Clone, Debug)]
pub(crate) struct Selection {
    pub root: PathBuf,
    pub source: PathBuf,
}

pub(crate) fn select(launch: &Path, requested: Option<&str>) -> Result<Selection> {
    let root = discover_project_root(launch)?;
    if root.parent().is_none() || home().ok().is_some_and(|home| root == home) {
        bail!(
            "start Nonaut inside a project directory, not {}",
            root.display()
        );
    }
    let source = match requested {
        Some(raw) => absolute(raw, launch)?,
        None => saved_path(&root)?,
    };
    if requested.is_none() && !source.exists() {
        let config = minimal_config(&root);
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            print!(
                "{}",
                crate::cli::render_project_setup(
                    config.name.as_deref().unwrap_or("project"),
                    &display_path(&root),
                    &display_path(&source),
                    &crate::runtime::integrations::detect_project(&root),
                    crate::cli::color_enabled(),
                )
            );
            io::stdout().flush()?;
            let mut answer = String::new();
            if io::stdin().read_line(&mut answer)? == 0 {
                bail!("project setup cancelled");
            }
            if !matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "" | "y" | "yes"
            ) {
                bail!("project setup cancelled");
            }
        }
        crate::config::write_json(&source, &config)?;
    }
    let loaded = load_file(&source, &root)?;
    crate::policy::validate_project(&root, &loaded)?;
    selected()
        .lock()
        .map_err(|_| anyhow::anyhow!("project selection lock poisoned"))?
        .insert(root.clone(), loaded.clone());
    Ok(Selection {
        root,
        source: loaded.source,
    })
}

fn selected() -> &'static Mutex<BTreeMap<PathBuf, Loaded>> {
    SELECTED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(crate) fn load(root: &Path) -> Result<Loaded> {
    let root = fs::canonicalize(root)?;
    if let Some(loaded) = selected()
        .lock()
        .map_err(|_| anyhow::anyhow!("project selection lock poisoned"))?
        .get(&root)
    {
        return Ok(loaded.clone());
    }
    let source = match std::env::var_os(CONFIG_ENV) {
        Some(path) => PathBuf::from(path),
        None => saved_path(&root)?,
    };
    if !source.exists() {
        if std::env::var_os(CONFIG_ENV).is_some() {
            bail!("project policy {} does not exist", source.display());
        }
        return Ok(Loaded {
            source,
            sources: Vec::new(),
            config: minimal_config(&root),
        });
    }
    let loaded = load_file(&source, &root)?;
    selected()
        .lock()
        .map_err(|_| anyhow::anyhow!("project selection lock poisoned"))?
        .insert(root, loaded.clone());
    Ok(loaded)
}

pub(crate) fn minimal_config(root: &Path) -> ProjectConfig {
    ProjectConfig {
        name: Some(inferred_name(root)),
        root: Some(root.to_string_lossy().into_owned()),
        runtime: Some(crate::runtime::RuntimeConfig {
            mode: Some(crate::runtime::Mode::Auto),
            ..Default::default()
        }),
        filesystem: FilesystemConfig {
            readwrite: vec![".".into()],
            ..Default::default()
        },
        network: Some(NetworkConfig::default()),
        ..Default::default()
    }
}

pub(crate) fn saved_path(root: &Path) -> Result<PathBuf> {
    let hash = Sha256::digest(root.as_os_str().as_encoded_bytes());
    let digest: String = hash.iter().take(16).map(|b| format!("{b:02x}")).collect();
    Ok(crate::config::config_dir()?
        .join("projects")
        .join(format!("{}-{digest}.json", inferred_name(root))))
}

pub(crate) fn load_file(source: &Path, root: &Path) -> Result<Loaded> {
    let source = fs::canonicalize(source)
        .with_context(|| format!("could not open policy {}", source.display()))?;
    let mut sources = Vec::new();
    let config = compose(
        &source,
        root,
        &mut BTreeSet::new(),
        &mut sources,
        &mut BTreeMap::new(),
        0,
    )?;
    Ok(Loaded {
        source,
        sources,
        config,
    })
}

fn compose(
    source: &Path,
    root: &Path,
    stack: &mut BTreeSet<PathBuf>,
    sources: &mut Vec<PathBuf>,
    cache: &mut BTreeMap<PathBuf, ProjectConfig>,
    depth: usize,
) -> Result<ProjectConfig> {
    if let Some(config) = cache.get(source) {
        return Ok(config.clone());
    }
    if depth > 32 || !stack.insert(source.to_path_buf()) {
        bail!(
            "cyclic or excessively deep policy import at {}",
            source.display()
        );
    }
    let text = fs::read_to_string(source)
        .with_context(|| format!("could not read {}", source.display()))?;
    let current: ProjectConfig = serde_json::from_str(&text)
        .with_context(|| format!("invalid policy {}", source.display()))?;
    if let Some(declared) = &current.root {
        let declared = fs::canonicalize(absolute(declared, root)?)
            .with_context(|| format!("invalid policy root {declared}"))?;
        if declared != root {
            bail!(
                "policy root {} does not match active project {}",
                declared.display(),
                root.display()
            );
        }
    }
    let mut merged = ProjectConfig::default();
    for import in &current.extends {
        if import.contains("://") {
            bail!("policy imports must be local files");
        }
        let path = fs::canonicalize(absolute(
            import,
            source.parent().context("policy has no parent")?,
        )?)
        .with_context(|| format!("could not resolve policy import {import}"))?;
        merge(
            &mut merged,
            compose(&path, root, stack, sources, cache, depth + 1)?,
        )?;
    }
    merge(&mut merged, current)?;
    stack.remove(source);
    if !sources.contains(&source.to_path_buf()) {
        sources.push(source.to_path_buf());
    }
    cache.insert(source.to_path_buf(), merged.clone());
    Ok(merged)
}

fn merge(base: &mut ProjectConfig, next: ProjectConfig) -> Result<()> {
    if next.name.is_some() {
        base.name = next.name;
    }
    if next.root.is_some() {
        base.root = next.root;
    }
    if let Some(next_runtime) = next.runtime {
        match &mut base.runtime {
            Some(runtime) => runtime.merge(next_runtime),
            None => base.runtime = Some(next_runtime),
        }
    }
    for (target, values) in [
        (&mut base.filesystem.read, next.filesystem.read),
        (&mut base.filesystem.readwrite, next.filesystem.readwrite),
        (&mut base.filesystem.deny, next.filesystem.deny),
    ] {
        for value in values {
            if !target.contains(&value) {
                target.push(value);
            }
        }
    }
    for (name, reference) in next.references {
        if let Some(existing) = base.references.get(&name)
            && (existing.path != reference.path || existing.access != reference.access)
        {
            bail!("conflicting definitions for reference {name}");
        }
        base.references.insert(name, reference);
    }
    if next.network.is_some() {
        base.network = next.network;
    }
    if next.tools.is_some() {
        base.tools = next.tools;
    }
    Ok(())
}

pub(crate) fn home() -> Result<PathBuf> {
    BaseDirs::new()
        .context("could not determine HOME")
        .map(|d| d.home_dir().to_path_buf())
}

pub(crate) fn absolute(raw: &str, root: &Path) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        bail!("path cannot be empty");
    }
    let path = match raw.strip_prefix("~/") {
        Some(rest) => home()?.join(rest),
        None if raw == "~" => home()?,
        None => PathBuf::from(raw),
    };
    Ok(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

pub(crate) fn display_policy_path(path: &Path) -> String {
    if path
        .extension()
        .is_some_and(|extension| extension == "json")
        && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
        && let Some((name, id)) = stem.rsplit_once('-')
        && !name.is_empty()
        && id.len() == 32
        && id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return display_path(&path.with_file_name(format!("{name}-<id>.json")));
    }
    display_path(path)
}

pub(crate) fn display_path(path: &Path) -> String {
    match home()
        .ok()
        .and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf))
    {
        Some(relative) if relative.as_os_str().is_empty() => "~".into(),
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

fn inferred_name(root: &Path) -> String {
    let name: String = root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    if name.is_empty() {
        "project".into()
    } else {
        name
    }
}

pub(crate) fn discover_project_root(launch: &Path) -> Result<PathBuf> {
    let launch = fs::canonicalize(launch)?;
    let boundary = home().ok().and_then(|home| fs::canonicalize(home).ok());
    Ok(discover_with_boundary(&launch, boundary.as_deref()))
}

fn discover_with_boundary(launch: &Path, boundary: Option<&Path>) -> PathBuf {
    for directory in launch.ancestors() {
        if boundary == Some(directory) {
            break;
        }
        if [
            ".git",
            "Cargo.toml",
            "go.mod",
            "package.json",
            "pyproject.toml",
            "requirements.txt",
            "bun.lock",
            "bun.lockb",
        ]
        .iter()
        .any(|marker| directory.join(marker).exists())
        {
            return directory.to_path_buf();
        }
    }
    launch.to_path_buf()
}

pub(crate) fn name_for_root(root: &Path) -> Option<String> {
    Some(
        load(root)
            .ok()?
            .config
            .name
            .unwrap_or_else(|| inferred_name(root)),
    )
}

pub(crate) fn reference_instructions(root: &Path) -> Option<String> {
    let loaded = load(root).ok()?;
    if loaded.config.references.is_empty() {
        return None;
    }
    Some(format!(
        "Configured references:\n{}",
        loaded
            .config
            .references
            .iter()
            .map(|(name, reference)| {
                format!(
                    "- {name}: {} ({:?})",
                    absolute(&reference.path, root)
                        .unwrap_or_default()
                        .display(),
                    reference.access
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn policy_labels_abbreviate_generated_ids_without_changing_custom_paths() {
        let source = PathBuf::from(format!("/policies/my-project-{}.json", "a".repeat(32)));
        assert_eq!(
            display_policy_path(&source),
            "/policies/my-project-<id>.json"
        );
        assert!(source.to_string_lossy().contains(&"a".repeat(32)));
        for source in [
            "/policies/rust-dev.json",
            "/policies/app-0123456789abcdef.json",
            "/policies/app-zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz.json",
            "/policies/app-0123456789abcdef0123456789abcdef.txt",
        ] {
            assert_eq!(display_policy_path(Path::new(source)), source);
        }
    }
    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nonaut-project-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }
    #[test]
    fn minimal_policy_has_no_external_grants() {
        let root = fixture();
        let value = serde_json::to_value(minimal_config(&root)).unwrap();
        assert_eq!(value["filesystem"], serde_json::json!({"readwrite":["."]}));
        assert_eq!(value["network"]["mode"], "blocked");
        assert!(value.get("references").is_none());
        assert!(value.get("extends").is_none());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn roots_are_optional_but_must_match_when_declared() {
        let root = fixture();
        let source = root.join("policy.json");
        fs::write(&source, "{}").unwrap();
        assert!(load_file(&source, &root).is_ok());
        fs::write(
            &source,
            serde_json::json!({"root": root.join("..")}).to_string(),
        )
        .unwrap();
        assert!(format!("{:#}", load_file(&source, &root).unwrap_err()).contains("does not match"));
        fs::write(&source, serde_json::json!({"root": root}).to_string()).unwrap();
        assert!(load_file(&source, &root).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn imports_compose_and_preserve_denials() {
        let root = fixture();
        fs::create_dir(root.join("policies")).unwrap();
        fs::write(root.join("policies/base.json"), r#"{"runtime":{"packs":["rust"]},"filesystem":{"deny":["./secret"]},"references":{"docs":{"path":"../docs","access":"read"}},"network":{"mode":"blocked"}}"#).unwrap();
        fs::write(root.join("project.json"), r#"{"extends":["./policies/base.json"],"runtime":{"packs":["rust","python"]},"filesystem":{"readwrite":["."]},"network":{"mode":"open"}}"#).unwrap();
        let loaded = load_file(&root.join("project.json"), &root).unwrap();
        assert_eq!(loaded.config.runtime.unwrap().packs, ["rust", "python"]);
        assert_eq!(loaded.config.filesystem.deny, ["./secret"]);
        assert_eq!(loaded.config.references["docs"].path, "../docs");
        assert_eq!(
            loaded.config.network.unwrap().mode,
            crate::config::NetworkMode::Open
        );
        assert_eq!(loaded.sources.len(), 2);
        fs::write(
            root.join("policies/base.json"),
            r#"{"extends":["../project.json"]}"#,
        )
        .unwrap();
        assert!(load_file(&root.join("project.json"), &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn discovery_uses_nearest_marker_and_home_boundary() {
        let root = fixture();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        let child = root.join("nested/src");
        fs::create_dir_all(&child).unwrap();
        assert_eq!(discover_with_boundary(&child, None), root);
        assert_eq!(discover_with_boundary(&child, Some(&root)), child);
        fs::write(root.join("nested/package.json"), "{}").unwrap();
        assert_eq!(discover_with_boundary(&child, None), root.join("nested"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn same_named_projects_have_distinct_stable_paths() {
        assert_ne!(
            saved_path(Path::new("/one/app")).unwrap(),
            saved_path(Path::new("/two/app")).unwrap()
        );
        assert_eq!(
            saved_path(Path::new("/one/app")).unwrap(),
            saved_path(Path::new("/one/app")).unwrap()
        );
    }

    #[test]
    fn imports_reject_conflicting_references_and_invalid_documents() {
        let root = fixture();
        fs::write(
            root.join("base.json"),
            r#"{"references":{"docs":{"path":"../docs","access":"read"}}}"#,
        )
        .unwrap();
        let source = root.join("project.json");
        fs::write(&source, r#"{"extends":["base.json"],"references":{"docs":{"path":"../other","access":"read"}}}"#).unwrap();
        assert!(
            format!("{:#}", load_file(&source, &root).unwrap_err())
                .contains("conflicting definitions")
        );
        fs::write(
            &source,
            r#"{"extends":["https://example.com/policy.json"]}"#,
        )
        .unwrap();
        assert!(format!("{:#}", load_file(&source, &root).unwrap_err()).contains("local files"));
        fs::write(&source, r#"{"filesystem":{"allow":["."]}}"#).unwrap();
        assert!(load_file(&source, &root).is_err());
        fs::write(&source, r#"{"extends":["missing.json"]}"#).unwrap();
        assert!(load_file(&source, &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diamond_imports_deduplicate_without_false_cycles() {
        let root = fixture();
        fs::write(
            root.join("base.json"),
            r#"{"runtime":{"packs":["rust"]},"filesystem":{"read":["src"]}}"#,
        )
        .unwrap();
        fs::write(root.join("left.json"), r#"{"extends":["base.json"]}"#).unwrap();
        fs::write(root.join("right.json"), r#"{"extends":["base.json"]}"#).unwrap();
        fs::write(
            root.join("project.json"),
            r#"{"extends":["left.json","right.json"]}"#,
        )
        .unwrap();
        let loaded = load_file(&root.join("project.json"), &root).unwrap();
        assert_eq!(loaded.sources.len(), 4);
        assert_eq!(loaded.config.runtime.unwrap().packs, ["rust"]);
        assert_eq!(loaded.config.filesystem.read, ["src"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn declared_roots_accept_symlink_aliases() {
        let root = fixture();
        let alias = root.with_extension("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        fs::write(
            root.join("project.json"),
            serde_json::json!({"root":alias}).to_string(),
        )
        .unwrap();
        assert!(load_file(&root.join("project.json"), &root).is_ok());
        assert_eq!(
            discover_project_root(&alias).unwrap(),
            discover_project_root(&root).unwrap()
        );
        fs::remove_file(alias).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
