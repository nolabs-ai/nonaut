use std::{
    collections::BTreeMap,
    env, fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Platform {
    Macos,
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Linux
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HostInventory {
    pub platform: Platform,
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub executable_paths: Vec<PathBuf>,
    pub homebrew: Vec<PathBuf>,
    pub developer_directories: Vec<PathBuf>,
    pub locations: BTreeMap<String, PathBuf>,
}

impl HostInventory {
    pub fn empty(home: &Path, workspace: &Path) -> Self {
        Self {
            platform: Platform::current(),
            home: home.to_path_buf(),
            workspace: workspace.to_path_buf(),
            executable_paths: Vec::new(),
            homebrew: Vec::new(),
            developer_directories: Vec::new(),
            locations: BTreeMap::new(),
        }
    }

    pub fn discover(home: &Path, workspace: &Path) -> Self {
        let mut host = Self::empty(home, workspace);
        let inherited = env::var_os("PATH")
            .map(|path| env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default();
        let mut prefixes = vec![
            PathBuf::from("/opt/homebrew"),
            PathBuf::from("/usr/local"),
            PathBuf::from("/home/linuxbrew/.linuxbrew"),
        ];
        if let Some(prefix) = env::var_os("HOMEBREW_PREFIX") {
            prefixes.push(prefix.into());
        }
        for bin in &inherited {
            if matches!(
                bin.file_name().and_then(|name| name.to_str()),
                Some("bin" | "sbin")
            ) && let Some(parent) = bin.parent()
            {
                prefixes.push(parent.to_path_buf());
            }
        }
        for prefix in prefixes {
            if prefix.join("Cellar").is_dir()
                && prefix.join("opt").is_dir()
                && host.trusted_installation(&prefix)
                && !host.homebrew.contains(&prefix)
            {
                host.homebrew.push(prefix);
            }
        }
        let mut bins = host
            .homebrew
            .iter()
            .flat_map(|prefix| [prefix.join("bin"), prefix.join("sbin")])
            .collect::<Vec<_>>();
        bins.extend(
            [
                "/usr/local/bin",
                "/usr/local/sbin",
                "/usr/bin",
                "/usr/sbin",
                "/bin",
                "/sbin",
            ]
            .map(PathBuf::from),
        );
        for bin in bins {
            if bin.is_dir()
                && host.trusted_installation(&bin)
                && !host.executable_paths.contains(&bin)
            {
                host.executable_paths.push(bin);
            }
        }
        for (name, fallback) in [
            ("CARGO_HOME", home.join(".cargo")),
            ("RUSTUP_HOME", home.join(".rustup")),
            ("PYENV_ROOT", home.join(".pyenv")),
            ("NVM_DIR", home.join(".nvm")),
            ("FNM_DIR", home.join(".fnm")),
            ("BUN_INSTALL", home.join(".bun")),
            ("GOROOT", PathBuf::from("/usr/local/go")),
        ] {
            let selected = env::var_os(name)
                .map(PathBuf::from)
                .filter(|path| host.trusted_installation(path))
                .unwrap_or(fallback);
            host.locations.insert(name.to_owned(), selected);
        }
        if host.platform == Platform::Macos {
            if let Some(path) = env::var_os("DEVELOPER_DIR").map(PathBuf::from)
                && path.is_dir()
                && host.trusted_installation(&path)
            {
                host.developer_directories.push(path);
            }
            #[cfg(target_os = "macos")]
            if let Ok(output) = std::process::Command::new("/usr/bin/xcode-select")
                .arg("-p")
                .env_clear()
                .output()
                && output.status.success()
            {
                let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
                if host.developer_directories.is_empty()
                    && path.is_dir()
                    && host.trusted_installation(&path)
                {
                    host.developer_directories.push(path);
                }
            }
        }
        host
    }

    pub fn location(&self, name: &str) -> &Path {
        &self.locations[name]
    }

    pub fn executable(&self, program: &str) -> Option<PathBuf> {
        self.executable_paths
            .iter()
            .map(|bin| bin.join(program))
            .find(|path| {
                path.is_file()
                    && self.trusted_installation(path)
                    && fs::metadata(path).is_ok_and(|metadata| metadata.mode() & 0o111 != 0)
            })
    }

    pub fn trusted_installation(&self, path: &Path) -> bool {
        let Ok(canonical) = fs::canonicalize(path) else {
            return false;
        };
        let workspace =
            fs::canonicalize(&self.workspace).unwrap_or_else(|_| self.workspace.clone());
        let home = fs::canonicalize(&self.home).unwrap_or_else(|_| self.home.clone());
        if !path.is_absolute()
            || canonical == home
            || canonical.parent().is_none()
            || canonical.starts_with(&workspace)
            || workspace.starts_with(&canonical)
        {
            return false;
        }
        let Ok(metadata) = fs::metadata(&canonical) else {
            return false;
        };
        let owner = fs::metadata(&home)
            .map(|metadata| metadata.uid())
            .unwrap_or(0);
        (metadata.uid() == 0 || metadata.uid() == owner) && metadata.mode() & 0o002 == 0
    }
}
