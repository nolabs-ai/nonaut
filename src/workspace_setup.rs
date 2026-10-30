use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::{
    config::{NetworkConfig, NetworkMode},
    workspace::{self, Selection},
};

const ACCENT: Color = Color::Rgb(232, 115, 74);
const MUTED: Color = Color::Rgb(136, 136, 136);

#[derive(Clone, Copy, Eq, PartialEq)]
enum Page {
    Landing,
    Select,
    Name,
    Profiles,
    Network,
    Domains,
    References,
    Default,
    Review,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum NetworkChoice {
    Inherit,
    Blocked,
    Allowlist,
    Open,
}

struct Wizard {
    launch: PathBuf,
    page: Page,
    landing: usize,
    configured: Vec<(String, PathBuf)>,
    selected_workspace: usize,
    name: String,
    profiles: Vec<(&'static str, bool, bool)>,
    profile_cursor: usize,
    network: NetworkChoice,
    network_cursor: usize,
    domains: String,
    references: String,
    make_default: bool,
    replace: Option<String>,
    error: Option<String>,
}

pub(crate) fn prompt(launch: &Path) -> Result<Selection> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return workspace::select(launch, Some("."));
    }
    let config = crate::config::load()?;
    if !config.workspace.prompt_on_unconfigured {
        return workspace::select(launch, Some("."));
    }
    let configured = config
        .workspace
        .definitions
        .iter()
        .map(|(name, definition)| Ok((name.clone(), workspace::workspace_root(definition)?)))
        .collect::<Result<Vec<_>>>()?;
    run(Wizard::new(launch.to_path_buf(), configured))
}

pub(crate) fn configure_current() -> Result<()> {
    require_terminal()?;
    let launch = std::env::current_dir().context("could not determine the working directory")?;
    let config = crate::config::load()?;
    let wizard = match workspace::config_for_root(&config, &launch)? {
        Some((name, definition)) => Wizard::edit(
            workspace::workspace_root(&definition)?,
            name,
            definition,
            config.workspace.default.as_deref(),
        ),
        None => Wizard::configure(launch),
    };
    let selection = run(wizard)?;
    println!(
        "Saved workspace {} at {}.",
        selection.name.as_deref().unwrap_or("workspace"),
        workspace::display_path(&selection.root)
    );
    Ok(())
}

pub(crate) fn edit(name: &str) -> Result<()> {
    require_terminal()?;
    workspace::validate_name(name)?;
    let config = crate::config::load()?;
    let definition = config
        .workspace
        .definitions
        .get(name)
        .cloned()
        .with_context(|| format!("workspace `{name}` is not configured"))?;
    let root = workspace::workspace_root(&definition)?;
    let selection = run(Wizard::edit(
        root,
        name.to_owned(),
        definition,
        config.workspace.default.as_deref(),
    ))?;
    println!(
        "Updated workspace {} at {}.",
        name,
        workspace::display_path(&selection.root)
    );
    Ok(())
}

fn require_terminal() -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("workspace setup needs an interactive terminal");
    }
    Ok(())
}

fn run(mut wizard: Wizard) -> Result<Selection> {
    let mut terminal = ratatui::init();
    let result = (|| loop {
        terminal.draw(|frame| wizard.render(frame))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(selection) = wizard.handle(key.code, key.modifiers)? {
            break Ok(selection);
        }
    })();
    ratatui::restore();
    result
}

impl Wizard {
    fn new(launch: PathBuf, configured: Vec<(String, PathBuf)>) -> Self {
        let inferred = launch
            .file_name()
            .and_then(|name| name.to_str())
            .map(safe_name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "workspace".to_owned());
        let profiles = crate::profiles::available_names()
            .into_iter()
            .map(|name| (name, false, detected(name, &launch)))
            .collect();
        Self {
            launch,
            page: Page::Landing,
            landing: 0,
            configured,
            selected_workspace: 0,
            name: inferred,
            profiles,
            profile_cursor: 0,
            network: NetworkChoice::Blocked,
            network_cursor: 1,
            domains: String::new(),
            references: String::new(),
            make_default: false,
            replace: None,
            error: None,
        }
    }

    fn configure(launch: PathBuf) -> Self {
        let mut wizard = Self::new(launch, Vec::new());
        wizard.page = Page::Name;
        wizard
    }

    fn edit(
        launch: PathBuf,
        name: String,
        definition: crate::config::WorkspaceConfig,
        default: Option<&str>,
    ) -> Self {
        let mut wizard = Self::configure(launch);
        wizard.name = name.clone();
        for (profile, selected, _) in &mut wizard.profiles {
            *selected = definition.profiles.iter().any(|name| name == profile);
        }
        wizard.references = definition
            .references
            .iter()
            .map(|(name, reference)| format!("{name}={}", reference.path))
            .collect::<Vec<_>>()
            .join(", ");
        let network = definition.network.unwrap_or_default();
        (wizard.network, wizard.network_cursor) = match network.mode {
            None if network.allow_domains.is_empty() => (NetworkChoice::Inherit, 0),
            None | Some(NetworkMode::Allowlist) => (NetworkChoice::Allowlist, 2),
            Some(NetworkMode::Blocked) => (NetworkChoice::Blocked, 1),
            Some(NetworkMode::Open) => (NetworkChoice::Open, 3),
        };
        wizard.domains = network.allow_domains.join(", ");
        wizard.make_default = default == Some(&name);
        wizard.replace = Some(name);
        wizard
    }

    fn handle(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Result<Option<Selection>> {
        self.error = None;
        if matches!(code, KeyCode::Char('c')) && modifiers.contains(KeyModifiers::CONTROL) {
            bail!("workspace setup cancelled");
        }
        match self.page {
            Page::Landing => self.handle_landing(code),
            Page::Select => self.handle_select(code),
            Page::Name => self.handle_text(code, Page::Profiles, TextField::Name),
            Page::Profiles => self.handle_profiles(code),
            Page::Network => self.handle_network(code),
            Page::Domains => self.handle_text(code, Page::References, TextField::Domains),
            Page::References => self.handle_text(code, Page::Default, TextField::References),
            Page::Default => self.handle_default(code),
            Page::Review => self.handle_review(code),
        }
    }

    fn handle_landing(&mut self, code: KeyCode) -> Result<Option<Selection>> {
        let choices = if self.configured.is_empty() { 3 } else { 4 };
        match code {
            KeyCode::Up => self.landing = self.landing.saturating_sub(1),
            KeyCode::Down => self.landing = (self.landing + 1).min(choices - 1),
            KeyCode::Enter => match self.landing {
                0 => return workspace::select(&self.launch, Some(".")).map(Some),
                1 => self.page = Page::Name,
                2 if !self.configured.is_empty() => self.page = Page::Select,
                _ => bail!("workspace setup cancelled"),
            },
            KeyCode::Esc => bail!("workspace setup cancelled"),
            _ => {}
        }
        Ok(None)
    }

    fn handle_select(&mut self, code: KeyCode) -> Result<Option<Selection>> {
        match code {
            KeyCode::Up => self.selected_workspace = self.selected_workspace.saturating_sub(1),
            KeyCode::Down => {
                self.selected_workspace =
                    (self.selected_workspace + 1).min(self.configured.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let name = &self.configured[self.selected_workspace].0;
                return workspace::select(&self.launch, Some(name)).map(Some);
            }
            KeyCode::Esc => self.page = Page::Landing,
            _ => {}
        }
        Ok(None)
    }

    fn handle_text(
        &mut self,
        code: KeyCode,
        next: Page,
        field: TextField,
    ) -> Result<Option<Selection>> {
        let value = match field {
            TextField::Name => &mut self.name,
            TextField::Domains => &mut self.domains,
            TextField::References => &mut self.references,
        };
        match code {
            KeyCode::Char(character) => value.push(character),
            KeyCode::Backspace => {
                value.pop();
            }
            KeyCode::Enter => {
                if field == TextField::Name
                    && let Err(error) = workspace::validate_name(value)
                {
                    self.error = Some(error.to_string());
                    return Ok(None);
                }
                if field == TextField::Domains && self.network == NetworkChoice::Allowlist {
                    let domains = split_csv(value);
                    if domains.is_empty() {
                        self.error = Some("enter at least one allowed domain".to_owned());
                        return Ok(None);
                    }
                }
                self.page = next;
            }
            KeyCode::Esc => {
                self.page = match field {
                    TextField::Name => Page::Landing,
                    TextField::Domains => Page::Network,
                    TextField::References => {
                        if self.network == NetworkChoice::Allowlist {
                            Page::Domains
                        } else {
                            Page::Network
                        }
                    }
                };
            }
            _ => {}
        }
        Ok(None)
    }

    fn handle_profiles(&mut self, code: KeyCode) -> Result<Option<Selection>> {
        match code {
            KeyCode::Up => self.profile_cursor = self.profile_cursor.saturating_sub(1),
            KeyCode::Down => {
                self.profile_cursor =
                    (self.profile_cursor + 1).min(self.profiles.len().saturating_sub(1));
            }
            KeyCode::Char(' ') => {
                self.profiles[self.profile_cursor].1 = !self.profiles[self.profile_cursor].1;
            }
            KeyCode::Enter => self.page = Page::Network,
            KeyCode::Esc => self.page = Page::Name,
            _ => {}
        }
        Ok(None)
    }

    fn handle_network(&mut self, code: KeyCode) -> Result<Option<Selection>> {
        match code {
            KeyCode::Up => self.network_cursor = self.network_cursor.saturating_sub(1),
            KeyCode::Down => self.network_cursor = (self.network_cursor + 1).min(3),
            KeyCode::Enter => {
                self.network = [
                    NetworkChoice::Inherit,
                    NetworkChoice::Blocked,
                    NetworkChoice::Allowlist,
                    NetworkChoice::Open,
                ][self.network_cursor];
                self.page = if self.network == NetworkChoice::Allowlist {
                    Page::Domains
                } else {
                    Page::References
                };
            }
            KeyCode::Esc => self.page = Page::Profiles,
            _ => {}
        }
        Ok(None)
    }

    fn handle_default(&mut self, code: KeyCode) -> Result<Option<Selection>> {
        match code {
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                self.make_default = !self.make_default;
            }
            KeyCode::Enter => self.page = Page::Review,
            KeyCode::Esc => self.page = Page::References,
            _ => {}
        }
        Ok(None)
    }

    fn handle_review(&mut self, code: KeyCode) -> Result<Option<Selection>> {
        match code {
            KeyCode::Enter => match self.save() {
                Ok(selection) => return Ok(Some(selection)),
                Err(error) => self.error = Some(format!("{error:#}")),
            },
            KeyCode::Esc => self.page = Page::Default,
            _ => {}
        }
        Ok(None)
    }

    fn save(&self) -> Result<Selection> {
        let profiles = self
            .profiles
            .iter()
            .filter(|(_, selected, _)| *selected)
            .map(|(name, _, _)| (*name).to_owned())
            .collect();
        let references = parse_references(&self.references)?;
        let network = (self.network != NetworkChoice::Inherit).then(|| NetworkConfig {
            mode: Some(match self.network {
                NetworkChoice::Inherit => unreachable!(),
                NetworkChoice::Blocked => NetworkMode::Blocked,
                NetworkChoice::Allowlist => NetworkMode::Allowlist,
                NetworkChoice::Open => NetworkMode::Open,
            }),
            allow_domains: if self.network == NetworkChoice::Allowlist {
                split_csv(&self.domains)
            } else {
                Vec::new()
            },
        });
        workspace::save(
            &self.name,
            &self.launch,
            profiles,
            references,
            network,
            self.make_default,
            self.replace.as_deref(),
        )?;
        workspace::select(&self.launch, Some(&self.name))
    }

    fn render(&self, frame: &mut Frame) {
        let area = centered(frame.area(), 76, 30);
        frame.render_widget(Clear, area);
        let block = Block::default()
            .title(Span::styled(
                " ⎔  Nonaut workspace setup ",
                Style::new().fg(ACCENT).bold(),
            ))
            .borders(Borders::ALL)
            .border_style(Style::new().fg(MUTED));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(inner);
        self.render_page(frame, rows[0]);
        let footer = self
            .error
            .as_deref()
            .unwrap_or("Enter continue · Esc back · Ctrl+C cancel");
        frame.render_widget(
            Paragraph::new(footer)
                .style(Style::new().fg(if self.error.is_some() {
                    Color::Red
                } else {
                    MUTED
                }))
                .alignment(Alignment::Center),
            rows[1],
        );
    }

    fn render_page(&self, frame: &mut Frame, area: Rect) {
        match self.page {
            Page::Landing => {
                let mut choices = vec![
                    "Start a safe one-time session",
                    "Configure and save this directory",
                ];
                if !self.configured.is_empty() {
                    choices.push("Select a configured workspace");
                }
                choices.push("Cancel");
                render_list(
                    frame,
                    area,
                    &format!(
                        "{} is not configured",
                        workspace::display_path(&self.launch)
                    ),
                    &choices,
                    self.landing,
                );
            }
            Page::Select => {
                let choices = self
                    .configured
                    .iter()
                    .map(|(name, path)| format!("{name}  {}", workspace::display_path(path)))
                    .collect::<Vec<_>>();
                render_list(
                    frame,
                    area,
                    "Select a workspace",
                    &choices,
                    self.selected_workspace,
                );
            }
            Page::Name => render_input(
                frame,
                area,
                "Workspace name",
                &self.name,
                "A short name used by --workspace",
            ),
            Page::Profiles => {
                let choices = self
                    .profiles
                    .iter()
                    .map(|(name, selected, detected)| {
                        format!(
                            "{} {name}{}",
                            if *selected { "[x]" } else { "[ ]" },
                            if *detected { "  detected" } else { "" }
                        )
                    })
                    .collect::<Vec<_>>();
                render_list(
                    frame,
                    area,
                    "Development profiles · Space toggles",
                    &choices,
                    self.profile_cursor,
                );
            }
            Page::Network => render_list(
                frame,
                area,
                "Network access",
                &[
                    "Inherit global policy",
                    "Blocked",
                    "HTTPS domain allowlist",
                    "Open — unrestricted",
                ],
                self.network_cursor,
            ),
            Page::Domains => render_input(
                frame,
                area,
                "Allowed HTTPS domains",
                &self.domains,
                "Comma-separated, for example index.crates.io, static.crates.io",
            ),
            Page::References => render_input(
                frame,
                area,
                "Read-only references",
                &self.references,
                "Optional comma-separated NAME=PATH entries",
            ),
            Page::Default => render_list(
                frame,
                area,
                "Use this workspace when Nonaut starts elsewhere?",
                &["No", "Yes"],
                usize::from(self.make_default),
            ),
            Page::Review => {
                let profiles = self
                    .profiles
                    .iter()
                    .filter(|(_, selected, _)| *selected)
                    .map(|(name, _, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ");
                let network = match self.network {
                    NetworkChoice::Inherit => "inherited",
                    NetworkChoice::Blocked => "blocked",
                    NetworkChoice::Allowlist => "allowlist",
                    NetworkChoice::Open => "open",
                };
                let lines = vec![
                    Line::from(vec![
                        Span::styled("Name        ", Style::new().fg(MUTED)),
                        Span::raw(&self.name),
                    ]),
                    Line::from(vec![
                        Span::styled("Root        ", Style::new().fg(MUTED)),
                        Span::raw(workspace::display_path(&self.launch)),
                        Span::styled("  read/write", Style::new().fg(ACCENT)),
                    ]),
                    Line::from(vec![
                        Span::styled("Profiles    ", Style::new().fg(MUTED)),
                        Span::raw(if profiles.is_empty() {
                            "none"
                        } else {
                            &profiles
                        }),
                    ]),
                    Line::from(vec![
                        Span::styled("Network     ", Style::new().fg(MUTED)),
                        Span::raw(network),
                    ]),
                    Line::from(vec![
                        Span::styled("References  ", Style::new().fg(MUTED)),
                        Span::raw(if self.references.trim().is_empty() {
                            "none"
                        } else {
                            &self.references
                        }),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Press Enter to save and launch.",
                        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
                    )),
                ];
                frame.render_widget(
                    Paragraph::new(lines)
                        .wrap(Wrap { trim: false })
                        .block(Block::default().title("Review").borders(Borders::NONE)),
                    area,
                );
            }
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TextField {
    Name,
    Domains,
    References,
}

fn render_list<T: AsRef<str>>(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    choices: &[T],
    selected: usize,
) {
    let items = choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let marker = if index == selected { "›" } else { " " };
            ListItem::new(format!("{marker} {}", choice.as_ref())).style(if index == selected {
                Style::new().fg(ACCENT).bold()
            } else {
                Style::default()
            })
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(Block::default().title(title).borders(Borders::NONE)),
        area,
    );
}

fn render_input(frame: &mut Frame, area: Rect, title: &str, value: &str, hint: &str) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(hint, Style::new().fg(MUTED))),
            Line::from(""),
            Line::from(format!("> {value}█")),
        ])
        .block(Block::default().title(title).borders(Borders::NONE))
        .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .collect()
}

fn detected(profile: &str, root: &Path) -> bool {
    let files: &[&str] = match profile {
        "rust" => &["Cargo.toml"],
        "python" => &["pyproject.toml", "requirements.txt"],
        "node" => &["package.json"],
        "go" => &["go.mod"],
        "bun" => &["bun.lock", "bun.lockb"],
        _ => &[],
    };
    files.iter().any(|file| root.join(file).is_file())
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_references(value: &str) -> Result<Vec<(String, PathBuf)>> {
    split_csv(value)
        .into_iter()
        .map(|entry| {
            let (name, path) = entry
                .split_once('=')
                .context("references must use NAME=PATH")?;
            workspace::validate_name(name)?;
            let path = if let Some(rest) = path.strip_prefix("~/") {
                directories::BaseDirs::new()
                    .context("could not determine the home directory")?
                    .home_dir()
                    .join(rest)
            } else {
                PathBuf::from(path)
            };
            let path = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()?.join(path)
            };
            Ok((name.to_owned(), path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_comma_separated_values_are_normalized() {
        assert_eq!(safe_name("My project!"), "Myproject");
        assert_eq!(split_csv(" one, two ,,three "), ["one", "two", "three"]);
    }

    #[test]
    fn references_require_valid_names_and_paths() {
        let references = parse_references("nono=../nono, docs=/tmp").unwrap();
        assert_eq!(references[0].0, "nono");
        assert!(references[0].1.is_absolute());
        assert_eq!(references[1], ("docs".to_owned(), PathBuf::from("/tmp")));
        assert!(parse_references("missing-path").is_err());
        assert!(parse_references("bad name=/tmp").is_err());
    }

    #[test]
    fn profile_detection_only_marks_matching_project_files() {
        let root = std::env::temp_dir().join(format!(
            "nonaut-workspace-profile-detection-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]").unwrap();
        assert!(detected("rust", &root));
        assert!(!detected("python", &root));
        std::fs::remove_dir_all(root).unwrap();
    }
}
