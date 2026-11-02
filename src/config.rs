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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(skip_serializing_if = "UiConfig::is_default")]
    pub ui: UiConfig,
    pub agent: AgentConfig,
    #[serde(skip_serializing_if = "ToolsConfig::is_default")]
    pub tools: ToolsConfig,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
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

impl UiConfig {
    fn is_default(&self) -> bool {
        self.theme == "dark"
    }
}
impl ToolsConfig {
    fn is_default(&self) -> bool {
        self.timeout_seconds == DEFAULT_TOOL_TIMEOUT_SECONDS
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProjectConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packs: Vec<String>,
    pub filesystem: FilesystemConfig,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub references: BTreeMap<String, ReferenceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<WorkspaceToolsConfig>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FilesystemConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub readwrite: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReferenceConfig {
    pub path: String,
    pub access: ReferenceAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReferenceAccess {
    Read,
    ReadWrite,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct NetworkConfig {
    pub mode: NetworkMode,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
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

#[cfg(any(
    feature = "provider-copilot",
    feature = "provider-openai-compatible-core"
))]
pub(crate) fn load_provider_config(provider: ProviderKind) -> Result<ProviderConfig> {
    Ok(load()?
        .agent
        .providers
        .get(provider.id())
        .cloned()
        .unwrap_or_default())
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

pub(crate) fn update(change: impl FnOnce(&mut Config)) -> Result<()> {
    let mut config = load()?;
    change(&mut config);
    write(&config)
}

fn write(config: &Config) -> Result<()> {
    write_json(&config_path()?, config)
}

pub(crate) fn write_json(path: &std::path::Path, value: &impl Serialize) -> Result<()> {
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
            serde_json::to_vec_pretty(value).context("could not encode Nonaut configuration")?;
        encoded.push(b'\n');
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
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
    fn application_settings_do_not_accept_project_policy() {
        for key in ["workspaces", "references", "sandbox", "filesystem"] {
            assert!(serde_json::from_value::<Config>(serde_json::json!({key: {}})).is_err());
        }
        let value = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(value, serde_json::json!({"agent": {"providers": {}}}));
    }
    #[test]
    fn project_policy_is_explicit_and_compact() {
        let policy: ProjectConfig = serde_json::from_value(serde_json::json!({
            "filesystem": {"readwrite": ["."]},
            "references": {"docs": {"path": "../docs", "access": "read"}}
        }))
        .unwrap();
        let value = serde_json::to_value(policy).unwrap();
        assert_eq!(value["filesystem"], serde_json::json!({"readwrite": ["."]}));
        assert!(value.get("packs").is_none());
        assert!(
            serde_json::from_value::<ProjectConfig>(serde_json::json!({
                "references": {"docs": {"path": "../docs"}}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_str::<ProjectConfig>(r#"{"filesystem":{"write":["."]}}"#).is_err()
        );
    }
    #[test]
    fn config_paths_follow_xdg() {
        assert_eq!(
            config_dir_from(Some(OsStr::new("/custom")), None).unwrap(),
            PathBuf::from("/custom/nonaut")
        );
        assert_eq!(
            config_dir_from(None, Some(OsStr::new("/home/me"))).unwrap(),
            PathBuf::from("/home/me/.config/nonaut")
        );
        assert!(config_dir_from(Some(OsStr::new("relative")), None).is_err());
    }

    #[test]
    fn atomic_writes_are_private_and_leave_no_temporary_files() {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("nonaut-config-write-{}", std::process::id()));
        let path = directory.join("project.json");
        write_json(&path, &serde_json::json!({"first":true})).unwrap();
        write_json(&path, &serde_json::json!({"second":true})).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(path).unwrap()).unwrap(),
            serde_json::json!({"second":true})
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
