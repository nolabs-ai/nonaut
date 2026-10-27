use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{ErrorKind, Write},
    os::unix::{fs::OpenOptionsExt, prelude::PermissionsExt},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::providers::ProviderKind;

const CONFIG_FILE: &str = "config.json";
const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 120;
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    pub ui: UiConfig,
    pub agent: AgentConfig,
    pub tools: ToolsConfig,
    pub sandbox: SandboxConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ui: UiConfig::default(),
            agent: AgentConfig::default(),
            tools: ToolsConfig::default(),
            sandbox: SandboxConfig {
                network: NetworkConfig {
                    mode: Some(NetworkMode::Blocked),
                    allow_domains: Vec::new(),
                },
                ..SandboxConfig::default()
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct UiConfig {
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "dark".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AgentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProviderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ToolsConfig {
    pub timeout_seconds: u64,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: DEFAULT_TOOL_TIMEOUT_SECONDS,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SandboxConfig {
    pub unconfigured_workspaces: UnconfiguredWorkspaces,
    pub network: NetworkConfig,
    pub filesystem: FilesystemConfig,
    pub workspaces: BTreeMap<PathBuf, WorkspaceConfig>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum UnconfiguredWorkspaces {
    #[default]
    Allow,
    Deny,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct NetworkConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<NetworkMode>,
    pub allow_domains: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NetworkMode {
    Open,
    #[default]
    Blocked,
    Allowlist,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FilesystemConfig {
    pub read: Vec<String>,
    pub write: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct WorkspaceConfig {
    pub profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<FilesystemConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<WorkspaceToolsConfig>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct WorkspaceToolsConfig {
    pub timeout_seconds: Option<u64>,
}

pub fn load_default_provider() -> Result<Option<ProviderKind>> {
    load()?
        .agent
        .provider
        .as_deref()
        .map(ProviderKind::parse)
        .transpose()
}

pub fn save_default_provider(provider: ProviderKind) -> Result<()> {
    update(|config| config.agent.provider = Some(provider.id().to_owned()))
}

pub fn load_model(provider: ProviderKind) -> Result<Option<String>> {
    Ok(load()?
        .agent
        .providers
        .get(provider.id())
        .and_then(|settings| settings.model.clone()))
}

pub fn save_model(provider: ProviderKind, model: &str) -> Result<()> {
    update(|config| {
        let settings = config
            .agent
            .providers
            .entry(provider.id().to_owned())
            .or_default();
        if settings.model.as_deref() != Some(model) {
            settings.reasoning = None;
        }
        settings.model = Some(model.to_owned());
    })
}

pub fn load_reasoning(provider: ProviderKind, model: &str) -> Result<Option<String>> {
    Ok(load()?
        .agent
        .providers
        .get(provider.id())
        .filter(|settings| settings.model.as_deref() == Some(model))
        .and_then(|settings| settings.reasoning.clone()))
}

pub fn save_reasoning(provider: ProviderKind, model: &str, effort: &str) -> Result<()> {
    update(|config| {
        let settings = config
            .agent
            .providers
            .entry(provider.id().to_owned())
            .or_default();
        settings.model = Some(model.to_owned());
        settings.reasoning = Some(effort.to_owned());
    })
}

pub fn load_theme() -> Result<Option<String>> {
    Ok(Some(load()?.ui.theme))
}

pub fn save_theme(theme: &str) -> Result<()> {
    update(|config| config.ui.theme = theme.to_owned())
}

pub(crate) fn load() -> Result<Config> {
    let path = config_path()?;
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("could not parse {}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let config = Config::default();
            write(&config)?;
            Ok(config)
        }
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

pub(crate) fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

pub(crate) fn config_dir() -> Result<PathBuf> {
    config_dir_from(
        env::var_os("XDG_CONFIG_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )
}

fn config_dir_from(xdg_config_home: Option<&OsStr>, home: Option<&OsStr>) -> Result<PathBuf> {
    let root = if let Some(directory) = xdg_config_home {
        let directory = PathBuf::from(directory);
        if !directory.is_absolute() {
            bail!("XDG_CONFIG_HOME must be an absolute path");
        }
        directory
    } else {
        PathBuf::from(home.context("HOME is not set; cannot place Nonaut configuration")?)
            .join(".config")
    };
    Ok(root.join("nonaut"))
}

fn update(change: impl FnOnce(&mut Config)) -> Result<()> {
    let mut config = load()?;
    change(&mut config);
    write(&config)
}

fn write(config: &Config) -> Result<()> {
    let path = config_path()?;
    let directory = path.parent().context("configuration path has no parent")?;
    fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(
        ".{CONFIG_FILE}.tmp-{}-{sequence}",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("could not create {}", temporary.display()))?;
        let mut encoded =
            serde_json::to_vec_pretty(config).context("could not encode Nonaut configuration")?;
        encoded.push(b'\n');
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)
            .with_context(|| format!("could not replace {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_provider() {
        assert_eq!(ProviderKind::parse("codex\n").unwrap(), ProviderKind::Codex);
        assert_eq!(ProviderKind::parse("claude").unwrap(), ProviderKind::Claude);
    }

    #[test]
    fn rejects_unknown_provider() {
        assert!(ProviderKind::parse("surprise").is_err());
    }

    #[test]
    fn default_config_is_one_readable_document() {
        let value = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(value["ui"]["theme"], "dark");
        assert_eq!(value["tools"]["timeout_seconds"], 120);
        assert_eq!(value["sandbox"]["unconfigured_workspaces"], "allow");
        assert_eq!(value["sandbox"]["network"]["mode"], "blocked");
        assert!(value["agent"].get("provider").is_none());
    }

    #[test]
    fn config_rejects_unknown_settings() {
        assert!(serde_json::from_str::<Config>(r#"{"mystery": true}"#).is_err());
        assert!(
            serde_json::from_str::<Config>(r#"{"ui":{"theme":"dark","mystery":true}}"#).is_err()
        );
    }

    #[test]
    fn xdg_config_home_is_used_on_every_platform() {
        assert_eq!(
            config_dir_from(
                Some(OsStr::new("/custom/config")),
                Some(OsStr::new("/home/me"))
            )
            .unwrap(),
            PathBuf::from("/custom/config/nonaut")
        );
    }

    #[test]
    fn config_falls_back_to_dot_config_under_home() {
        assert_eq!(
            config_dir_from(None, Some(OsStr::new("/home/me"))).unwrap(),
            PathBuf::from("/home/me/.config/nonaut")
        );
    }

    #[test]
    fn relative_xdg_config_home_is_rejected() {
        assert!(config_dir_from(Some(OsStr::new("relative")), None).is_err());
    }
}
