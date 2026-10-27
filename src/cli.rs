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

pub(crate) fn render_help(color: bool) -> String {
    let title = styled("⎔  nonaut", ACCENT, true, color);
    let subtitle = styled("security-first agent harness", MUTED, false, color);
    let heading = |value| styled(value, MUTED, true, color);
    let command = |value| styled(value, ACCENT, false, color);

    let commands = [
        (
            "exec [--cwd DIR] -- <command>",
            "Run a command in the nono sandbox",
        ),
        ("ps", "List persistent sessions"),
        ("attach <session>", "Attach to a persistent session"),
        ("kill <session>", "Stop a persistent session"),
        ("kill --all, -a", "Stop all persistent sessions"),
        ("help, --help, -h", "Show this help"),
        ("version, --version, -V", "Show the version"),
    ]
    .map(|(name, description)| {
        let padded = format!("{name:<27}");
        format!("  {} {description}", styled(&padded, ACCENT, false, color))
    })
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
    fn plain_help_lists_every_cli_command() {
        let help = render_help(false);
        for command in ["exec", "ps", "attach", "kill", "help"] {
            assert!(help.contains(command));
        }
        assert!(help.contains("/help"));
        assert!(!help.contains("\u{1b}["));
    }
}
