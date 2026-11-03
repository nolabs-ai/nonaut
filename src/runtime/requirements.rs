use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Access {
    Read,
    Write,
    ReadWrite,
}

impl Access {
    pub fn writable(self) -> bool {
        self != Self::Read
    }
}

impl std::fmt::Display for Access {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::ReadWrite => "read/write",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GrantKind {
    Runtime,
    PrivateState,
    Executor,
    Explicit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Grant {
    pub path: PathBuf,
    pub canonical_path: Option<PathBuf>,
    pub access: Access,
    pub kind: GrantKind,
    pub source: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub(crate) struct Requirements {
    pub read_paths: Vec<PathBuf>,
    pub write_paths: Vec<PathBuf>,
    pub write_only_paths: Vec<PathBuf>,
    pub path_entries: Vec<PathBuf>,
    pub environment: BTreeMap<String, String>,
    pub private_directories: Vec<PathBuf>,
}

pub(crate) struct Context<'a> {
    pub home: &'a Path,
    pub workspace: &'a Path,
    pub command_home: &'a Path,
}

impl Context<'_> {
    pub fn cache(&self, pack: &str) -> Result<PathBuf> {
        crate::runtime_paths::workspace_cache(self.home, &["profiles", pack], self.workspace)
    }
}

impl Requirements {
    pub fn private_directory(&mut self, path: PathBuf) -> PathBuf {
        if !self.private_directories.contains(&path) {
            self.private_directories.push(path.clone());
        }
        path
    }

    pub fn cache(&mut self, context: &Context<'_>, pack: &str) -> Result<PathBuf> {
        let cache = self.private_directory(context.cache(pack)?);
        self.write_paths.push(cache.clone());
        Ok(cache)
    }

    pub fn read_existing(&mut self, path: PathBuf) {
        if path.exists() && !self.read_paths.contains(&path) {
            self.read_paths.push(path);
        }
    }

    pub fn bin(&mut self, path: PathBuf) {
        if path.is_dir() && !self.path_entries.contains(&path) {
            self.read_existing(path.clone());
            self.path_entries.push(path);
        }
    }
}
