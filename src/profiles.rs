//! Development-tool profiles resolved into sandbox capabilities.
//!
//! Profiles describe toolchain access only. Workspace filesystem, network and
//! timeout policy remain owned by `policy` and are layered around this plan.

use std::{
    collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher},
    env, fs,
    hash::{Hash, Hasher},
    os::unix::fs::PermissionsExt,
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
];

pub(crate) fn resolve(names: &[String], home: &Path, workspace: &Path) -> Result<ProfilePlan> {
    let mut combined = ProfilePlan::default();
    let mut selected = BTreeSet::new();
    for name in names {
        if !selected.insert(name.as_str()) {
            bail!("workspace profile `{name}` is listed more than once");
        }
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
    let cache = profile_cache("rust", workspace)?;
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
    let cache = profile_cache("python", workspace)?;
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

fn add_existing(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if path.exists() {
        paths.push(path);
    }
}

fn add_path_entry(plan: &mut ProfilePlan, path: PathBuf) {
    if path.is_dir() {
        plan.path_entries.push(path);
    }
}

fn profile_cache(profile: &str, workspace: &Path) -> Result<PathBuf> {
    let root = env::var_os("XDG_CACHE_HOME").map_or_else(
        || home_dir().map(|home| home.join(".cache")),
        |path| Ok(PathBuf::from(path)),
    )?;
    if !root.is_absolute() {
        bail!("XDG_CACHE_HOME must be an absolute path");
    }
    let mut hasher = DefaultHasher::new();
    workspace.hash(&mut hasher);
    let cache = root
        .join("nonaut")
        .join("profiles")
        .join(profile)
        .join(format!("{:016x}", hasher.finish()));
    fs::create_dir_all(&cache).with_context(|| format!("could not create {}", cache.display()))?;
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("could not protect {}", cache.display()))?;
    Ok(cache)
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot place profile cache")
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
        assert_eq!(names, ["rust", "python"]);
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
}
