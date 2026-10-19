//! Optional sandbox policy: tool network mode, extra read-only paths and an
//! HTTPS host allowlist.
//!
//! The file lives in nonaut's config directory, never in the workspace: the
//! sandbox grants the agent write access to the workspace, so a policy stored
//! there could be rewritten by the agent to widen its own sandbox.
//!
//! ```json
//! {
//!   "network": "allowlist",
//!   "allow_hosts": ["crates.io", "*.crates.io"],
//!   "read_paths": ["~/.cargo/registry"],
//!   "workspaces": {
//!     "/home/me/project": { "allow_hosts": ["registry.npmjs.org"] }
//!   }
//! }
//! ```
//!
//! `network` is `open` (the default), `blocked`, or `allowlist`; listing
//! `allow_hosts` alone implies `allowlist`. Top-level entries apply
//! everywhere; a `workspaces` entry whose path equals the workspace adds
//! hosts and paths and may override `network`.

use std::{
    collections::HashMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use nono::net_filter::validate_host_pattern;
use serde::Deserialize;

/// How sandboxed tools may use the network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Network {
    #[default]
    Open,
    Blocked,
    Allowlist,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rules {
    #[serde(default)]
    network: Option<Network>,
    #[serde(default)]
    tool_timeout_seconds: Option<u64>,
    #[serde(default)]
    allow_hosts: Vec<String>,
    #[serde(default)]
    read_paths: Vec<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    network: Option<Network>,
    #[serde(default)]
    tool_timeout_seconds: Option<u64>,
    #[serde(default)]
    allow_hosts: Vec<String>,
    #[serde(default)]
    read_paths: Vec<String>,
    #[serde(default)]
    workspaces: HashMap<PathBuf, Rules>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Policy {
    pub network: Network,
    /// Upper bound for any tool call, `run_command` and `/exec`.
    pub tool_timeout_seconds: u64,
    pub allow_hosts: Vec<String>,
    pub read_paths: Vec<PathBuf>,
}

pub(crate) const DEFAULT_TOOL_TIMEOUT_SECONDS: u64 = 120;
const MAX_TOOL_TIMEOUT_SECONDS: u64 = 3600;

impl Default for Policy {
    fn default() -> Self {
        Self {
            network: Network::Open,
            tool_timeout_seconds: DEFAULT_TOOL_TIMEOUT_SECONDS,
            allow_hosts: Vec::new(),
            read_paths: Vec::new(),
        }
    }
}

pub(crate) fn path() -> Result<PathBuf> {
    Ok(crate::config::config_dir()?.join("policy.json"))
}

pub(crate) fn load(workspace: &Path) -> Result<Policy> {
    let path = path()?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Policy::default()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    let home = BaseDirs::new()
        .context("could not determine the home directory")?
        .home_dir()
        .to_path_buf();
    parse(&text, workspace, &home).with_context(|| format!("invalid {}", path.display()))
}

fn parse(text: &str, workspace: &Path, home: &Path) -> Result<Policy> {
    let file: File = serde_json::from_str(text)?;
    let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mut rules = Rules {
        network: file.network,
        tool_timeout_seconds: file.tool_timeout_seconds,
        allow_hosts: file.allow_hosts,
        read_paths: file.read_paths,
    };
    if let Some(extra) = file.workspaces.iter().find_map(|(path, rules)| {
        let path = expand_home(&path.to_string_lossy(), home);
        (fs::canonicalize(&path).unwrap_or(path) == workspace).then_some(rules)
    }) {
        rules.network = extra.network.or(rules.network);
        rules.tool_timeout_seconds = extra.tool_timeout_seconds.or(rules.tool_timeout_seconds);
        rules.allow_hosts.extend(extra.allow_hosts.iter().cloned());
        rules.read_paths.extend(extra.read_paths.iter().cloned());
    }

    let network = match (rules.network, rules.allow_hosts.is_empty()) {
        (None, true) => Network::Open,
        (None, false) | (Some(Network::Allowlist), false) => Network::Allowlist,
        (Some(Network::Allowlist), true) => {
            bail!("\"network\": \"allowlist\" needs at least one allow_hosts entry")
        }
        (Some(mode), false) => {
            let mode = if mode == Network::Open {
                "open"
            } else {
                "blocked"
            };
            bail!("allow_hosts only applies with \"network\": \"allowlist\", not \"{mode}\"")
        }
        (Some(mode), true) => mode,
    };
    let tool_timeout_seconds = rules
        .tool_timeout_seconds
        .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECONDS);
    if !(1..=MAX_TOOL_TIMEOUT_SECONDS).contains(&tool_timeout_seconds) {
        bail!("tool_timeout_seconds must be between 1 and {MAX_TOOL_TIMEOUT_SECONDS}");
    }
    for host in &rules.allow_hosts {
        if host == "*" {
            bail!("allow_hosts may not contain \"*\"; list the hosts the sandbox needs");
        }
        validate_host_pattern(host).map_err(anyhow::Error::msg)?;
    }
    let read_paths = rules
        .read_paths
        .iter()
        .map(|raw| {
            let path = expand_home(raw, home);
            if path.is_absolute() {
                Ok(path)
            } else {
                bail!("read_paths entry `{raw}` must be absolute or start with ~/")
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Policy {
        network,
        tool_timeout_seconds,
        allow_hosts: rules.allow_hosts,
        read_paths,
    })
}

fn expand_home(raw: &str, home: &Path) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if raw == "~" => home.to_path_buf(),
        None => PathBuf::from(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_matching_workspace_rules_and_expands_home() {
        let policy = parse(
            r#"{
                "allow_hosts": ["crates.io"],
                "read_paths": ["~/.cargo/registry"],
                "workspaces": {
                    "/work/app": { "allow_hosts": ["*.npmjs.org"] },
                    "/work/other": { "allow_hosts": ["example.com"] }
                }
            }"#,
            Path::new("/work/app"),
            Path::new("/home/me"),
        )
        .unwrap();
        assert_eq!(policy.network, Network::Allowlist);
        assert_eq!(policy.allow_hosts, ["crates.io", "*.npmjs.org"]);
        assert_eq!(
            policy.read_paths,
            [PathBuf::from("/home/me/.cargo/registry")]
        );
    }

    #[test]
    fn rejects_allow_all_bad_patterns_relative_paths_and_typos() {
        let home = Path::new("/home/me");
        let workspace = Path::new("/work");
        assert!(parse(r#"{"allow_hosts": ["*"]}"#, workspace, home).is_err());
        assert!(parse(r#"{"allow_hosts": ["foo*bar.com"]}"#, workspace, home).is_err());
        assert!(parse(r#"{"read_paths": ["relative/dir"]}"#, workspace, home).is_err());
        assert!(parse(r#"{"allow_host": ["crates.io"]}"#, workspace, home).is_err());
        assert!(parse(r#"{"network": "allowlist"}"#, workspace, home).is_err());
        assert!(
            parse(
                r#"{"network": "open", "allow_hosts": ["a.com"]}"#,
                workspace,
                home
            )
            .is_err()
        );
        assert!(parse(r#"{"network": "offline"}"#, workspace, home).is_err());
    }

    #[test]
    fn tool_timeout_defaults_validates_and_can_be_overridden_per_workspace() {
        let home = Path::new("/home/me");
        let work = Path::new("/work");
        assert_eq!(parse("{}", work, home).unwrap().tool_timeout_seconds, 120);
        let policy = parse(
            r#"{"tool_timeout_seconds": 600, "workspaces": {"/work": {"tool_timeout_seconds": 900}}}"#,
            work,
            home,
        )
        .unwrap();
        assert_eq!(policy.tool_timeout_seconds, 900);
        assert!(parse(r#"{"tool_timeout_seconds": 0}"#, work, home).is_err());
        assert!(parse(r#"{"tool_timeout_seconds": 3601}"#, work, home).is_err());
    }

    #[test]
    fn network_is_open_by_default_and_workspaces_can_override_it() {
        let home = Path::new("/home/me");
        let policy = parse("{}", Path::new("/work"), home).unwrap();
        assert_eq!(policy, Policy::default());
        assert_eq!(policy.network, Network::Open);
        let policy = parse(
            r#"{"network": "blocked", "workspaces": {"/work": {"network": "open"}}}"#,
            Path::new("/work"),
            home,
        )
        .unwrap();
        assert_eq!(policy.network, Network::Open);
    }
}
