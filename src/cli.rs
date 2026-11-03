use std::io::{self, IsTerminal};

use crossterm::style::{Color, Stylize};

pub(crate) const ACCENT: Color = Color::Rgb {
    r: 232,
    g: 115,
    b: 74,
};
pub(crate) const SUCCESS: Color = Color::Rgb {
    r: 126,
    g: 198,
    b: 153,
};
const ERROR: Color = Color::Rgb {
    r: 224,
    g: 108,
    b: 117,
};
pub(crate) const MUTED: Color = Color::Rgb {
    r: 136,
    g: 136,
    b: 136,
};

pub(crate) fn color_enabled() -> bool {
    io::stdout().is_terminal() && terminal_colors_enabled()
}

fn terminal_colors_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::env::var("TERM").is_ok_and(|term| term != "dumb")
}

pub(crate) fn styled(value: &str, color: Color, bold: bool, enabled: bool) -> String {
    if !enabled {
        return value.to_owned();
    }
    if bold {
        format!("{}", value.with(color).bold())
    } else {
        format!("{}", value.with(color))
    }
}

pub(crate) fn success(message: &str) {
    let color = color_enabled();
    println!(
        "{} {}",
        styled("◆", SUCCESS, true, color),
        styled(message, SUCCESS, false, color)
    );
}

pub(crate) fn notice(message: &str, detail: Option<&str>) {
    let color = color_enabled();
    println!(
        "{} {}",
        styled("◇", ACCENT, true, color),
        styled(message, ACCENT, false, color)
    );
    if let Some(detail) = detail {
        println!("  {}", styled(detail, MUTED, false, color));
    }
}

pub(crate) fn render_project_setup(
    name: &str,
    root: &str,
    source: &str,
    packs: &[String],
    color: bool,
) -> String {
    let heading = |text| styled(text, MUTED, true, color);
    let packs = if packs.is_empty() {
        "none".to_owned()
    } else {
        format!("{} · isolated caches", packs.join(", "))
    };
    let (directory, filename) = source
        .rsplit_once('/')
        .map_or(("", source), |(directory, filename)| (directory, filename));
    let location = if directory.is_empty() {
        format!("    {filename}")
    } else {
        format!("    {directory}/\n    {filename}")
    };
    format!(
        "\n{}\n{}\n\n  {}\n  {}\n\n{}\n    Project     read/write\n    Runtime     auto · installed tools and project markers\n    Detected    {packs}\n    Network     blocked\n    References  none\n\n{}\n{location}\n\n{}\n\n{} Create policy and launch? {} ",
        styled("⎔  nonaut", ACCENT, true, color),
        styled("Set up this project", MUTED, false, color),
        styled(name, ACCENT, true, color),
        root,
        heading("  ACCESS"),
        heading("  SAVE POLICY"),
        styled(
            "  Edit the policy file whenever your workflow needs more access.",
            MUTED,
            false,
            color
        ),
        styled("  ›", ACCENT, true, color),
        styled("[Y/n]", ACCENT, true, color),
    )
}

pub(crate) fn render_help(color: bool) -> String {
    let title = styled("⎔  nonaut", ACCENT, true, color);
    let subtitle = styled("security-first agent harness", MUTED, false, color);
    let heading = |value| styled(value, MUTED, true, color);
    let command = |value| styled(value, ACCENT, false, color);

    let commands = vec![
        ("--config FILE", "Use a project policy file"),
        (
            "exec [--cwd DIR] -- <command>",
            "Run a command in the nono sandbox",
        ),
        ("config", "Show the current project configuration"),
        (
            "runtime [show|check|list]",
            "Inspect runtime grants and compatibility",
        ),
        ("ps", "List persistent sessions"),
        ("attach <session>", "Attach to a persistent session"),
        ("kill <session>", "Stop a persistent session"),
        ("kill --all, -a", "Stop all persistent sessions"),
        ("help, --help, -h", "Show this help"),
        ("version, --version, -V", "Show the version"),
    ];
    #[cfg(feature = "control-plane")]
    let commands = {
        let mut commands = commands;
        commands.splice(
            1..1,
            [
                (
                    "enroll --url URL --token TOKEN",
                    "Enroll for audit delivery",
                ),
                ("unenroll", "Remove local enrollment credentials"),
                ("audit <status|sync>", "Inspect or retry audit delivery"),
            ],
        );
        commands
    };
    let commands = commands
        .into_iter()
        .map(|(name, description)| {
            let padded = format!("{name:<27}");
            format!("  {} {description}", styled(&padded, ACCENT, false, color))
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "{title}\n{subtitle}\n\n{}\n  {}\n  {}\n\n{}\n{commands}\n\n{}\n  Start the TUI and type {} to view in-session commands.\n",
        heading("USAGE"),
        command("nonaut"),
        command("nonaut <command> [options]"),
        heading("COMMANDS"),
        heading("TUI"),
        command("/help"),
    )
}

pub(crate) fn print_help() {
    print!("{}", render_help(color_enabled()));
}

pub(crate) fn print_version() {
    let color = color_enabled();
    println!(
        "{} {}",
        styled("nonaut", ACCENT, true, color),
        env!("CARGO_PKG_VERSION")
    );
}

pub(crate) fn print_command_help(command: &str, usage: &str, description: &str) {
    let color = color_enabled();
    println!(
        "{}  {}\n{}\n\n{}\n  {}\n",
        styled("⎔", ACCENT, true, color),
        styled(&format!("nonaut {command}"), ACCENT, true, color),
        styled(description, MUTED, false, color),
        styled("USAGE", MUTED, true, color),
        styled(usage, ACCENT, false, color),
    );
}

pub(crate) fn print_error(error: &anyhow::Error) {
    let color = io::stderr().is_terminal() && terminal_colors_enabled();
    eprintln!(
        "{} {}",
        styled("◆", ERROR, true, color),
        styled(&format!("{error:#}"), ERROR, false, color)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_setup_preserves_the_same_information_without_color() {
        let render = |color| {
            render_project_setup(
                "service",
                "~/Work/service",
                "~/.config/nonaut/projects/service-123.json",
                &["rust".into(), "python".into()],
                color,
            )
        };
        let plain = render(false);
        for text in [
            "service",
            "~/Work/service",
            "read/write",
            "rust, python · isolated caches",
            "blocked",
            "References  none",
            "~/.config/nonaut/projects/",
            "service-123.json",
            "Create policy and launch? [Y/n]",
        ] {
            assert!(plain.contains(text), "missing {text}");
        }
        assert!(!plain.contains('\u{1b}'));
        let colored = render(true);
        assert!(colored.contains("\u{1b}[1m"));
        let mut unstyled = String::new();
        let mut escape = false;
        for ch in colored.chars() {
            if ch == '\u{1b}' {
                escape = true;
            } else if escape {
                escape = ch != 'm';
            } else {
                unstyled.push(ch);
            }
        }
        assert_eq!(unstyled, plain);
    }

    #[test]
    fn project_setup_handles_no_packs_and_a_bare_policy_filename() {
        let rendered = render_project_setup("app", "/work/app", "policy.json", &[], false);
        assert!(rendered.contains("Detected    none"));
        assert!(rendered.contains("Runtime     auto"));
        assert!(rendered.contains("    policy.json\n"));
        assert!(!rendered.contains("isolated caches"));
    }

    #[test]
    fn plain_help_lists_every_cli_command() {
        let help = render_help(false);
        for command in ["exec", "config", "ps", "attach", "kill", "help"] {
            assert!(help.contains(command));
        }
        #[cfg(feature = "control-plane")]
        for command in ["enroll", "unenroll", "audit"] {
            assert!(help.contains(command));
        }
        #[cfg(not(feature = "control-plane"))]
        for command in ["enroll", "unenroll", "audit"] {
            assert!(!help.contains(command));
        }
        assert!(help.contains("/help"));
        assert!(!help.contains("\u{1b}["));
    }
}
