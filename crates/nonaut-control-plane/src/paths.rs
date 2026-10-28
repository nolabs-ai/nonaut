use std::{env, path::PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Clone)]
pub(crate) struct Paths {
    pub enrollment: PathBuf,
    pub key: PathBuf,
    pub sessions: PathBuf,
    pub outbox: PathBuf,
}

impl Paths {
    pub fn from_environment() -> Result<Self> {
        let home = env::var_os("HOME").context("HOME is not set")?;
        let config = absolute_root("XDG_CONFIG_HOME")?
            .unwrap_or_else(|| PathBuf::from(&home).join(".config"))
            .join("nonaut");
        let state = absolute_root("XDG_STATE_HOME")?
            .unwrap_or_else(|| PathBuf::from(home).join(".local/state"))
            .join("nonaut");
        Ok(Self {
            enrollment: config.join("control-plane.json"),
            key: state.join("control-plane/device.pk8"),
            sessions: state.join("audit"),
            outbox: state.join("control-plane-outbox"),
        })
    }
}

fn absolute_root(name: &str) -> Result<Option<PathBuf>> {
    let Some(value) = env::var_os(name) else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        bail!("{name} must be an absolute path");
    }
    Ok(Some(path))
}
