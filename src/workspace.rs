use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;

use crate::config::{Config, NetworkConfig, ReferenceConfig, WorkspaceConfig};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Selection {
    pub name: Option<String>,
    pub root: PathBuf,
    pub configured: bool,
}

impl Selection {
    fn ad_hoc(root: PathBuf) -> Self {
        Self {
            name: None,
            root,
            configured: false,
        }
    }
}

pub(crate) fn select(launch_dir: &Path, requested: Option<&str>) -> Result<Selection> {
    select_from(&crate::config::load()?, launch_dir, requested)
}

fn select_from(config: &Config, launch_dir: &Path, requested: Option<&str>) -> Result<Selection> {
    let launch_dir = canonical_directory(launch_dir)?;
    if requested == Some(".") {
        return Ok(Selection::ad_hoc(launch_dir));
    }

    if let Some(name) = requested {
        return configured(config, name);
    }

    let mut matching: Option<(usize, String, PathBuf)> = None;
    for (name, workspace) in &config.workspace.definitions {
        let root = workspace_root(workspace)?;
        if launch_dir.starts_with(&root) {
            let depth = root.components().count();
            if matching
                .as_ref()
                .is_none_or(|(current, _, _)| depth > *current)
            {
                matching = Some((depth, name.clone(), root));
            }
        }
    }
    if let Some((_, name, root)) = matching {
        return Ok(Selection {
            name: Some(name),
            root,
            configured: true,
        });
    }

    if let Some(name) = config.workspace.default.as_deref() {
        return configured(config, name)
            .with_context(|| format!("default workspace `{name}` is invalid"));
    }

    Ok(Selection::ad_hoc(launch_dir))
}

fn configured(config: &Config, name: &str) -> Result<Selection> {
    validate_name(name)?;
    let workspace = config
        .workspace
        .definitions
        .get(name)
        .with_context(|| format!("workspace `{name}` is not configured"))?;
    Ok(Selection {
        name: Some(name.to_owned()),
        root: workspace_root(workspace)?,
        configured: true,
    })
}

pub(crate) fn config_for_root(
    config: &Config,
    root: &Path,
) -> Result<Option<(String, WorkspaceConfig)>> {
    let root = canonical_directory(root)?;
    let mut found = None;
    for (name, workspace) in &config.workspace.definitions {
        let configured_root = workspace_root(workspace)?;
        if configured_root == root {
            if found.is_some() {
                bail!("multiple workspaces resolve to {}", root.display());
            }
            found = Some((name.clone(), workspace.clone()));
        }
    }
    Ok(found)
}

pub(crate) fn workspace_root(workspace: &WorkspaceConfig) -> Result<PathBuf> {
    if workspace.path.trim().is_empty() {
        bail!("workspace path cannot be empty");
    }
    canonical_directory(&expand_home(&workspace.path)?)
        .with_context(|| format!("invalid workspace path `{}`", workspace.path))
}

pub(crate) fn reference_root(raw: &str) -> Result<PathBuf> {
    if raw.trim().is_empty() {
        bail!("reference path cannot be empty");
    }
    canonical_directory(&expand_home(raw)?)
        .with_context(|| format!("invalid reference path `{raw}`"))
}

pub(crate) fn display_path(path: &Path) -> String {
    let Some(home) = BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) else {
        return path.display().to_string();
    };
    match path.strip_prefix(home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("workspace names may contain only letters, numbers, '-' and '_'");
    }
    Ok(())
}

pub(crate) fn save(
    name: &str,
    root: &Path,
    profiles: Vec<String>,
    references: Vec<(String, PathBuf)>,
    network: Option<NetworkConfig>,
    make_default: bool,
    replace: Option<&str>,
) -> Result<()> {
    validate_name(name)?;
    let root = canonical_directory(root)?;
    let mut reference_map = std::collections::BTreeMap::new();
    let mut reference_paths = Vec::new();
    for (reference_name, path) in references {
        validate_name(&reference_name)?;
        let path = canonical_directory(&path)?;
        if path.starts_with(&root) || root.starts_with(&path) {
            bail!("reference `{reference_name}` overlaps the writable workspace");
        }
        if reference_paths.contains(&path) {
            bail!("reference `{reference_name}` duplicates another reference path");
        }
        reference_paths.push(path.clone());
        reference_map.insert(
            reference_name,
            ReferenceConfig {
                path: display_path(&path),
            },
        );
    }
    crate::profiles::validate_names(&profiles)?;
    if let Some(network) = &network {
        crate::policy::validate_network_config(network)?;
    }
    let config = crate::config::load()?;
    if config.workspace.definitions.contains_key(name) && replace != Some(name) {
        bail!("workspace `{name}` already exists; use `nonaut workspace edit {name}`");
    }
    for (configured_name, configured) in &config.workspace.definitions {
        if replace != Some(configured_name) && workspace_root(configured)? == root {
            bail!(
                "{} is already configured as workspace `{configured_name}`",
                root.display()
            );
        }
    }
    crate::config::update(|config| {
        if let Some(previous) = replace {
            config.workspace.definitions.remove(previous);
            if config.workspace.default.as_deref() == Some(previous) {
                config.workspace.default = None;
            }
        }
        config.workspace.definitions.insert(
            name.to_owned(),
            WorkspaceConfig {
                path: display_path(&root),
                profiles,
                references: reference_map,
                network,
                ..WorkspaceConfig::default()
            },
        );
        if make_default {
            config.workspace.default = Some(name.to_owned());
        }
    })
}

pub(crate) fn run_command(args: Vec<String>) -> Result<()> {
    match args.as_slice() {
        [] => {
            print_help();
            Ok(())
        }
        [command] if matches!(command.as_str(), "help" | "--help" | "-h") => {
            print_help();
            Ok(())
        }
        [command] if command == "list" => list(),
        [command, name] if command == "show" => show(name),
        [command, name] if command == "remove" => remove(name),
        [command, name] if command == "default" => set_default(name),
        [command, rest @ ..] if command == "add" => add(rest),
        [command, name] if command == "edit" => crate::workspace_setup::edit(name),
        [command, ..] => bail!("unknown workspace command `{command}`"),
    }
}

fn list() -> Result<()> {
    let config = crate::config::load()?;
    if config.workspace.definitions.is_empty() {
        println!("No configured workspaces.");
        return Ok(());
    }
    for (name, workspace) in &config.workspace.definitions {
        let default = if config.workspace.default.as_deref() == Some(name) {
            " (default)"
        } else {
            ""
        };
        println!("{name}{default}\t{}", workspace.path);
    }
    Ok(())
}

fn show(name: &str) -> Result<()> {
    let config = crate::config::load()?;
    let workspace = config
        .workspace
        .definitions
        .get(name)
        .with_context(|| format!("workspace `{name}` is not configured"))?;
    println!("Workspace  {name}");
    println!("Root       {}", workspace.path);
    println!(
        "Default    {}",
        if config.workspace.default.as_deref() == Some(name) {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "Profiles   {}",
        if workspace.profiles.is_empty() {
            "none".to_owned()
        } else {
            workspace.profiles.join(", ")
        }
    );
    let network = match workspace.network.as_ref().and_then(|network| network.mode) {
        None if workspace
            .network
            .as_ref()
            .is_some_and(|network| !network.allow_domains.is_empty()) =>
        {
            "allowlist"
        }
        None => "inherited",
        Some(crate::config::NetworkMode::Blocked) => "blocked",
        Some(crate::config::NetworkMode::Allowlist) => "allowlist",
        Some(crate::config::NetworkMode::Open) => "open",
    };
    println!("Network    {network}");
    if let Some(network) = &workspace.network
        && !network.allow_domains.is_empty()
    {
        println!("Domains    {}", network.allow_domains.join(", "));
    }
    println!(
        "Timeout    {}",
        workspace
            .tools
            .as_ref()
            .and_then(|tools| tools.timeout_seconds)
            .map_or_else(|| "inherited".to_owned(), |seconds| format!("{seconds}s"))
    );
    if workspace.references.is_empty() {
        println!("References none");
    } else {
        for (reference, config) in &workspace.references {
            println!("Reference  {reference} -> {} (read-only)", config.path);
        }
    }
    if let Some(filesystem) = &workspace.filesystem {
        for path in &filesystem.read {
            println!("Read       {path}");
        }
        for path in &filesystem.write {
            println!("Write      {path}");
        }
    }
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    validate_name(name)?;
    let mut removed = false;
    crate::config::update(|config| {
        removed = config.workspace.definitions.remove(name).is_some();
        if config.workspace.default.as_deref() == Some(name) {
            config.workspace.default = None;
        }
    })?;
    if !removed {
        bail!("workspace `{name}` is not configured");
    }
    println!("Removed workspace {name}.");
    Ok(())
}

fn set_default(name: &str) -> Result<()> {
    validate_name(name)?;
    if !crate::config::load()?
        .workspace
        .definitions
        .contains_key(name)
    {
        bail!("workspace `{name}` is not configured");
    }
    crate::config::update(|config| {
        config.workspace.default = Some(name.to_owned());
    })?;
    println!("Default workspace set to {name}.");
    Ok(())
}

fn add(args: &[String]) -> Result<()> {
    let mut path = None;
    let mut name = None;
    let mut profiles = Vec::new();
    let mut references = Vec::new();
    let mut make_default = false;
    let mut arguments = args.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--name" => name = arguments.next().cloned(),
            "--profile" => profiles.push(
                arguments
                    .next()
                    .context("--profile needs a profile name")?
                    .clone(),
            ),
            "--reference" => {
                let value = arguments.next().context("--reference needs NAME=PATH")?;
                let (reference_name, reference_path) = value
                    .split_once('=')
                    .context("--reference needs NAME=PATH")?;
                references.push((reference_name.to_owned(), absolute(reference_path)?));
            }
            "--default" => make_default = true,
            value if value.starts_with('-') => bail!("unknown workspace add option `{value}`"),
            value if path.is_none() => path = Some(absolute(value)?),
            value => bail!("unexpected workspace add argument `{value}`"),
        }
    }
    let path = path.unwrap_or(std::env::current_dir()?);
    let inferred = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .context("could not infer a workspace name; use --name")?;
    let name = name.as_deref().unwrap_or(inferred);
    save(name, &path, profiles, references, None, make_default, None)?;
    println!("Saved workspace {name} at {}.", display_path(&path));
    Ok(())
}

fn absolute(raw: &str) -> Result<PathBuf> {
    let path = expand_home(raw)?;
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn print_help() {
    println!(
        "Workspace configuration\n\n\
         USAGE\n  nonaut workspace list\n  nonaut workspace show NAME\n  \
         nonaut workspace add [PATH] [--name NAME] [--profile NAME] \
         [--reference NAME=PATH] [--default]\n  nonaut workspace default NAME\n  \
         nonaut workspace edit NAME\n  nonaut workspace remove NAME"
    );
}

pub(crate) fn reference_instructions(root: &Path) -> Option<String> {
    let config = crate::config::load().ok()?;
    let (_, workspace) = config_for_root(&config, root).ok()??;
    if workspace.references.is_empty() {
        return None;
    }
    let references = workspace
        .references
        .iter()
        .map(|(name, reference)| format!("- {name}: {} (read-only)", reference.path))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Configured reference workspaces are available for context but cannot be modified:\n{references}"
    ))
}

fn expand_home(raw: &str) -> Result<PathBuf> {
    match raw.strip_prefix("~/") {
        Some(rest) => Ok(BaseDirs::new()
            .context("could not determine the home directory")?
            .home_dir()
            .join(rest)),
        None if raw == "~" => Ok(BaseDirs::new()
            .context("could not determine the home directory")?
            .home_dir()
            .to_path_buf()),
        None => Ok(PathBuf::from(raw)),
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("workspace paths must be absolute or start with ~/");
    }
    let path =
        fs::canonicalize(path).with_context(|| format!("could not resolve {}", path.display()))?;
    if !path.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{WorkspaceConfig, WorkspaceSettings};

    fn config(definitions: impl IntoIterator<Item = (&'static str, PathBuf)>) -> Config {
        let mut config = Config {
            workspace: WorkspaceSettings::default(),
            ..Config::default()
        };
        for (name, path) in definitions {
            config.workspace.definitions.insert(
                name.to_owned(),
                WorkspaceConfig {
                    path: path.to_string_lossy().into_owned(),
                    ..WorkspaceConfig::default()
                },
            );
        }
        config
    }

    #[test]
    fn current_directory_is_an_ad_hoc_workspace_without_configuration() {
        let directory = std::env::current_dir().unwrap();
        let selected = select_from(&Config::default(), &directory, None).unwrap();
        assert_eq!(selected.root, directory.canonicalize().unwrap());
        assert!(!selected.configured);
    }

    #[test]
    fn explicit_name_and_most_specific_containing_workspace_are_selected() {
        let root =
            std::env::temp_dir().join(format!("nonaut-workspace-selection-{}", std::process::id()));
        let parent = root.join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();
        let config = config([("parent", parent.clone()), ("child", child.clone())]);

        let selected = select_from(&config, &child, None).unwrap();
        assert_eq!(selected.name.as_deref(), Some("child"));
        assert_eq!(
            select_from(&config, &parent, Some("child")).unwrap().root,
            child.canonicalize().unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_is_used_only_when_launch_directory_matches_nothing() {
        let root =
            std::env::temp_dir().join(format!("nonaut-workspace-default-{}", std::process::id()));
        let project = root.join("project");
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        let mut config = config([("project", project.clone())]);
        config.workspace.default = Some("project".to_owned());
        assert_eq!(
            select_from(&config, &elsewhere, None).unwrap().root,
            project.canonicalize().unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
