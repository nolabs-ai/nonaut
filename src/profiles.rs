//! Development-tool profiles resolved into sandbox capabilities.
//!
//! Profiles describe toolchain access only. Workspace filesystem, network and
//! timeout policy remain owned by `policy` and are layered around this plan.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct ProfilePlan {
    pub read_paths: Vec<PathBuf>,
    pub write_paths: Vec<PathBuf>,
    pub path_entries: Vec<PathBuf>,
    pub environment: BTreeMap<String, String>,
}

type ProfileResolver = fn(&Path, &Path) -> Result<ProfilePlan>;

struct ProfileDefinition {
    name: &'static str,
    resolve: ProfileResolver,
}

/// The single source of truth for built-in development profiles.
const PROFILES: &[ProfileDefinition] = &[
    ProfileDefinition {
        name: "rust",
        resolve: rust,
    },
    ProfileDefinition {
        name: "python",
        resolve: python,
    },
    ProfileDefinition {
        name: "node",
        resolve: node,
    },
    ProfileDefinition {
        name: "go",
        resolve: go,
    },
    ProfileDefinition {
        name: "bun",
        resolve: bun,
    },
];

pub(crate) fn resolve(names: &[String], home: &Path, workspace: &Path) -> Result<ProfilePlan> {
    let mut selected = BTreeSet::new();
    for name in names {
        if !selected.insert(name.as_str()) {
            bail!("workspace profile `{name}` is listed more than once");
        }
    }
    let mut combined = ProfilePlan::default();
    for name in names {
        combined.merge(name, resolve_one(name, home, workspace)?)?;
    }
    Ok(combined)
}

fn resolve_one(name: &str, home: &Path, workspace: &Path) -> Result<ProfilePlan> {
    let definition = PROFILES.iter().find(|profile| profile.name == name);
    let Some(definition) = definition else {
        let available = PROFILES
            .iter()
            .map(|profile| profile.name)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("unsupported workspace profile `{name}`; available profiles: {available}");
    };
    (definition.resolve)(home, workspace)
}

impl ProfilePlan {
    fn merge(&mut self, profile: &str, other: Self) -> Result<()> {
        extend_unique(&mut self.read_paths, other.read_paths);
        extend_unique(&mut self.write_paths, other.write_paths);
        extend_unique(&mut self.path_entries, other.path_entries);
        for (name, value) in other.environment {
            if let Some(previous) = self.environment.get(&name) {
                if previous != &value {
                    bail!(
                        "workspace profile `{profile}` conflicts with another selected profile's `{name}` environment setting"
                    );
                }
            } else {
                self.environment.insert(name, value);
            }
        }
        Ok(())
    }
}

fn extend_unique(target: &mut Vec<PathBuf>, paths: Vec<PathBuf>) {
    for path in paths {
        if !target.contains(&path) {
            target.push(path);
        }
    }
}

fn rust(home: &Path, workspace: &Path) -> Result<ProfilePlan> {
    let cargo = home.join(".cargo");
    let cache = profile_cache(home, "rust", workspace)?;
    let mut plan = ProfilePlan::default();
    add_existing(&mut plan.read_paths, cargo.join("bin"));
    add_existing(&mut plan.read_paths, home.join(".rustup"));
    add_path_entry(&mut plan, cargo.join("bin"));
    plan.write_paths.push(cache.clone());
    plan.environment.insert(
        "CARGO_HOME".to_owned(),
        cache.to_string_lossy().into_owned(),
    );
    plan.environment.insert(
        "RUSTUP_HOME".to_owned(),
        home.join(".rustup").to_string_lossy().into_owned(),
    );
    Ok(plan)
}

fn python(home: &Path, workspace: &Path) -> Result<ProfilePlan> {
    let cache = profile_cache(home, "python", workspace)?;
    let mut plan = ProfilePlan::default();
    for path in [
        home.join(".pyenv"),
        home.join(".local/bin"),
        home.join(".local/share/uv"),
    ] {
        add_existing(&mut plan.read_paths, path);
    }
    for path in [
        home.join(".pyenv/shims"),
        home.join(".pyenv/bin"),
        home.join(".local/bin"),
    ] {
        add_path_entry(&mut plan, path);
    }
    plan.write_paths.push(cache.clone());
    for name in ["PIP_CACHE_DIR", "UV_CACHE_DIR", "PYTHONPYCACHEPREFIX"] {
        plan.environment
            .insert(name.to_owned(), cache.to_string_lossy().into_owned());
    }
    plan.environment.insert(
        "PYENV_ROOT".to_owned(),
        home.join(".pyenv").to_string_lossy().into_owned(),
    );
    Ok(plan)
}

fn node(home: &Path, workspace: &Path) -> Result<ProfilePlan> {
    let cache = profile_cache(home, "node", workspace)?;
    let nvm = home.join(".nvm");
    let fnm = home.join(".fnm");
    let fnm_data = home.join(".local/share/fnm");
    let mut plan = ProfilePlan::default();
    for path in [
        &nvm,
        &fnm,
        &fnm_data,
        &home.join(".node"),
        &home.join(".npm-global"),
        &home.join("Library/pnpm"),
        &home.join(".local/share/pnpm"),
    ] {
        add_existing(&mut plan.read_paths, path.to_path_buf());
    }

    // Prefer the version already active in the trusted parent shell, then
    // expose installed versions as a fallback for GUI and minimal shells.
    add_active_path_entries(&mut plan, &[&nvm, &fnm, &fnm_data]);
    add_child_bins(&mut plan, &nvm.join("versions/node"), Path::new("bin"));
    add_child_bins(
        &mut plan,
        &fnm.join("node-versions"),
        Path::new("installation/bin"),
    );
    add_child_bins(
        &mut plan,
        &fnm_data.join("node-versions"),
        Path::new("installation/bin"),
    );
    for path in [
        home.join(".node/bin"),
        home.join(".npm-global/bin"),
        home.join("Library/pnpm"),
        home.join(".local/share/pnpm"),
    ] {
        add_path_entry(&mut plan, path);
    }

    plan.write_paths.push(cache.clone());
    for (name, directory) in [
        ("npm_config_cache", "npm"),
        ("YARN_CACHE_FOLDER", "yarn"),
        ("COREPACK_HOME", "corepack"),
        ("npm_config_store_dir", "pnpm-store"),
    ] {
        let path = cache_directory(&cache, directory)?;
        plan.environment
            .insert(name.to_owned(), path.to_string_lossy().into_owned());
    }
    if nvm.is_dir() {
        plan.environment
            .insert("NVM_DIR".to_owned(), nvm.to_string_lossy().into_owned());
    }
    Ok(plan)
}

fn go(home: &Path, workspace: &Path) -> Result<ProfilePlan> {
    let cache = profile_cache(home, "go", workspace)?;
    let private_gopath = cache_directory(&cache, "gopath")?;
    let mut plan = ProfilePlan::default();
    for path in [home.join("go/bin"), PathBuf::from("/usr/local/go")] {
        add_existing(&mut plan.read_paths, path);
    }
    for path in [
        home.join("go/bin"),
        PathBuf::from("/usr/local/go/bin"),
        cache_directory(&private_gopath, "bin")?,
    ] {
        add_path_entry(&mut plan, path);
    }
    plan.write_paths.push(cache.clone());
    for (name, path) in [
        ("GOPATH", private_gopath),
        ("GOCACHE", cache_directory(&cache, "build")?),
        ("GOMODCACHE", cache_directory(&cache, "mod")?),
    ] {
        plan.environment
            .insert(name.to_owned(), path.to_string_lossy().into_owned());
    }
    plan.environment
        .insert("GOENV".to_owned(), "off".to_owned());
    Ok(plan)
}

fn bun(home: &Path, workspace: &Path) -> Result<ProfilePlan> {
    let cache = profile_cache(home, "bun", workspace)?;
    let install = home.join(".bun");
    let mut plan = ProfilePlan::default();
    add_existing(&mut plan.read_paths, install.clone());
    add_path_entry(&mut plan, install.join("bin"));
    plan.write_paths.push(cache.clone());
    plan.environment.insert(
        "BUN_INSTALL_CACHE_DIR".to_owned(),
        cache.to_string_lossy().into_owned(),
    );
    Ok(plan)
}

fn add_existing(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if path.exists() {
        paths.push(path);
    }
}

fn add_path_entry(plan: &mut ProfilePlan, path: PathBuf) {
    if path.is_dir() && !plan.path_entries.contains(&path) {
        plan.path_entries.push(path);
    }
}

fn add_active_path_entries(plan: &mut ProfilePlan, roots: &[&Path]) {
    let canonical_roots = roots
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect::<Vec<_>>();
    let Some(path) = env::var_os("PATH") else {
        return;
    };
    for entry in env::split_paths(&path) {
        let Ok(canonical) = fs::canonicalize(&entry) else {
            continue;
        };
        if canonical_roots
            .iter()
            .any(|root| canonical.starts_with(root))
        {
            add_path_entry(plan, canonical);
        }
    }
}

/// Add one executable directory below each immediate child of `parent`.
/// Canonical containment prevents a version-directory symlink escaping the
/// profile's already-approved toolchain root.
fn add_child_bins(plan: &mut ProfilePlan, parent: &Path, suffix: &Path) {
    let Ok(canonical_parent) = fs::canonicalize(parent) else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let mut bins = entries
        .flatten()
        .filter_map(|entry| fs::canonicalize(entry.path().join(suffix)).ok())
        .filter(|path| path.is_dir() && path.starts_with(&canonical_parent))
        .collect::<Vec<_>>();
    bins.sort_unstable_by(|left, right| right.cmp(left));
    for bin in bins {
        add_path_entry(plan, bin);
    }
}

fn cache_directory(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = parent.join(name);
    fs::create_dir_all(&path).with_context(|| format!("could not create {}", path.display()))?;
    Ok(path)
}

fn profile_cache(home: &Path, profile: &str, workspace: &Path) -> Result<PathBuf> {
    let cache = crate::runtime_paths::workspace_cache(home, &["profiles", profile], workspace)?;
    crate::runtime_paths::prepare_private_directory(&cache)?;
    Ok(cache)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_the_expected_unique_profiles() {
        let names = PROFILES
            .iter()
            .map(|profile| profile.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["rust", "python", "node", "go", "bun"]);
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn rejects_unknown_profiles_with_available_names() {
        let error = resolve(
            &["ruby".to_owned()],
            Path::new("/home/me"),
            Path::new("/work"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("python"));
        assert!(error.contains("rust"));
    }

    #[test]
    fn rejects_duplicate_profiles() {
        let error = resolve(
            &["rust".to_owned(), "rust".to_owned()],
            Path::new("/home/me"),
            Path::new("/work"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("listed more than once"));
    }

    #[test]
    fn version_discovery_does_not_follow_bins_outside_its_root() {
        use std::os::unix::fs::symlink;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/profile-tests")
            .join(format!("{}-{unique}", std::process::id()));
        let versions = directory.join("versions");
        let inside = versions.join("v2/bin");
        let outside = directory.join("outside/bin");
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(directory.join("outside"), versions.join("v1")).unwrap();

        let mut plan = ProfilePlan::default();
        add_child_bins(&mut plan, &versions, Path::new("bin"));

        assert_eq!(plan.path_entries, [fs::canonicalize(inside).unwrap()]);
        fs::remove_dir_all(directory).unwrap();
    }
}
