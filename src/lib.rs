mod cli;
mod config;
mod mcp;
mod policy;
mod profiles;
mod providers;
mod proxy;
mod sandbox;
mod sessions;
mod tools;
mod tui;
mod undo;

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
};

use tui::App;

pub fn run_sandbox() -> Result<()> {
    tools::run_sandbox().context("sandboxed tool failed")
}

pub async fn run_mcp(workspace: std::path::PathBuf) -> Result<()> {
    mcp::run(workspace).await.context("MCP server failed")
}

pub async fn run(args: Vec<String>) -> Result<()> {
    sessions::run_cli(args).await
}

pub fn print_help() {
    cli::print_help();
}

pub fn print_version() {
    cli::print_version();
}

pub fn print_command_help(command: &str, usage: &str, description: &str) {
    cli::print_command_help(command, usage, description);
}

pub fn print_error(error: &anyhow::Error) {
    cli::print_error(error);
}

pub async fn run_exec(cwd: std::path::PathBuf, command: String) -> Result<String> {
    let session_root =
        std::env::current_dir().context("could not determine the working directory")?;
    tools::execute_shell(&session_root, &cwd, command).await
}

pub async fn run_session_worker(id: String, workspace: std::path::PathBuf) -> Result<()> {
    sessions::run_worker(id, workspace).await
}

pub async fn run_tui() -> Result<()> {
    let cwd = std::env::current_dir().context("could not determine the working directory")?;
    tui::init_theme();
    let no_auto_connect = std::env::var_os("NONAUT_NO_AUTO_CONNECT").is_some();
    let mut app = if no_auto_connect {
        App::new_shell(cwd)
    } else {
        let (default_provider, startup_warning) = match config::load_default_provider() {
            Ok(provider) => (provider, None),
            Err(error) => (
                None,
                Some(format!("Could not load the saved provider: {error:#}")),
            ),
        };
        App::new(cwd, default_provider, startup_warning)
    };
    app.enable_undo();
    let mut terminal = ratatui::init();
    let keyboard_enhancements = execute!(
        std::io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok();
    let mouse_capture = execute!(std::io::stdout(), EnableMouseCapture).is_ok();
    // Without bracketed paste every newline in a paste arrives as Enter and
    // submits a fragment of it.
    let bracketed_paste = execute!(std::io::stdout(), EnableBracketedPaste).is_ok();
    let result = app.run(&mut terminal).await;
    if bracketed_paste {
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    }
    if mouse_capture {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    if keyboard_enhancements {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    ratatui::restore();
    result.context("terminal error")?;
    Ok(())
}
