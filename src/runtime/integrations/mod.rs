mod apple_sdk;
mod bun;
mod gcc;
mod git;
mod go;
mod homebrew;
mod node;
mod python;
mod rust;
mod system;

use super::{
    inventory::HostInventory,
    requirements::{Context, Requirements},
};
use anyhow::Result;
use serde::Serialize;

type Resolver = fn(&HostInventory, &Context<'_>) -> Result<Requirements>;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Category {
    Toolchain,
    Host,
    System,
}

#[derive(Serialize)]
pub(crate) struct Probe {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

#[derive(Serialize)]
pub(crate) struct Integration {
    pub name: &'static str,
    pub category: Category,
    pub description: &'static str,
    pub probe: Option<Probe>,
    pub markers: &'static [&'static str],
    #[serde(skip)]
    detect: fn(&HostInventory) -> bool,
    #[serde(skip)]
    resolve: Resolver,
}

pub(crate) const CATALOG: &[Integration] = &[
    system::DEFINITION,
    homebrew::DEFINITION,
    apple_sdk::DEFINITION,
    gcc::DEFINITION,
    git::DEFINITION,
    rust::DEFINITION,
    python::DEFINITION,
    node::DEFINITION,
    go::DEFINITION,
    bun::DEFINITION,
];

impl Integration {
    pub fn requirements(
        &self,
        host: &HostInventory,
        context: &Context<'_>,
    ) -> Result<Requirements> {
        (self.resolve)(host, context)
    }

    pub fn detected(&self, host: &HostInventory) -> bool {
        (self.detect)(host)
    }

    pub fn project_marker(&self, root: &std::path::Path) -> bool {
        self.markers
            .iter()
            .any(|marker| root.join(marker).is_file())
    }
}

pub(crate) fn detect_project(root: &std::path::Path) -> Vec<String> {
    CATALOG
        .iter()
        .filter(|pack| pack.project_marker(root))
        .map(|pack| pack.name.to_owned())
        .collect()
}
