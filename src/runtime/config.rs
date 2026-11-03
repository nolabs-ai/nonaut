use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::FilesystemConfig;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    #[default]
    Auto,
    Manual,
    Custom,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Custom => "custom",
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RuntimeConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(skip_serializing_if = "FilesystemConfig::is_empty")]
    pub filesystem: FilesystemConfig,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
}

impl RuntimeConfig {
    pub fn merge(&mut self, next: Self) {
        let denies = self.filesystem.deny.clone();
        if next.mode.is_some() {
            *self = next;
            append_unique(&mut self.filesystem.deny, denies);
            return;
        }
        append_unique(&mut self.packs, next.packs);
        append_unique(&mut self.exclude, next.exclude);
        append_unique(&mut self.filesystem.read, next.filesystem.read);
        append_unique(&mut self.filesystem.readwrite, next.filesystem.readwrite);
        append_unique(&mut self.filesystem.deny, next.filesystem.deny);
        self.environment.extend(next.environment);
    }
}

fn append_unique<T: PartialEq>(target: &mut Vec<T>, values: Vec<T>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}
