mod cli;
mod config;
mod control_plane;
mod mcp;
mod policy;
mod profiles;
mod providers;
mod proxy;
mod runtime_paths;
mod sandbox;
mod sessions;
mod tools;
mod tui;
mod undo;
mod workspace;
mod workspace_setup;

use anyhow::{Context, Result};
use crossterm::{
    cursor::MoveTo,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{Clear, ClearType},
};

use tui::App;

pub fn run_sandbox() -> Result<()> {
    tools::run_sandbox().context("sandboxed tool failed")
}

pub async fn run_mcp(workspace: std::path::PathBuf) -> Result<()> {
    tools::prewarm(&workspace)
        .await
        .context("could not prepare the workspace sandbox")?;
    mcp::run(workspace).await.context("MCP server failed")
}

pub async fn run(args: Vec<String>, requested_workspace: Option<String>) -> Result<()> {
    sessions::run_cli(args, requested_workspace.as_deref()).await
}

#[cfg(feature = "control-plane")]
pub async fn run_enroll(args: Vec<String>) -> Result<()> {
    control_plane::run_enroll(args).await
}

#[cfg(feature = "control-plane")]
pub fn run_unenroll(args: Vec<String>) -> Result<()> {
    control_plane::run_unenroll(args)
}

#[cfg(feature = "control-plane")]
pub async fn run_audit(args: Vec<String>) -> Result<()> {
    control_plane::run_audit(args).await
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

pub fn workspace_root(requested: Option<&str>) -> Result<std::path::PathBuf> {
    let launch = std::env::current_dir().context("could not determine the working directory")?;
    Ok(workspace::select(&launch, requested)?.root)
}

pub fn run_workspace_command(args: Vec<String>) -> Result<()> {
    workspace::run_command(args)
}

pub fn run_workspace_setup() -> Result<()> {
    workspace_setup::configure_current()
}

pub async fn run_exec(
    session_root: std::path::PathBuf,
    cwd: std::path::PathBuf,
    command: String,
) -> Result<String> {
    let mut audit =
        control_plane::start_session(&session_root, None, None, None).unwrap_or_else(|error| {
            eprintln!("Warning: could not start the local audit session: {error:#}");
            None
        });
    let call_id = "exec";
    let mut tool_context = tools::audit_context(
        call_id,
        &session_root,
        "run_command",
        &serde_json::json!({
            "program": "sh",
            "args": ["-c", command],
            "cwd": cwd,
        }),
    );
    tool_context.tool = "exec".to_owned();
    if let Some(Err(error)) = control_plane::tool_started(&mut audit, &tool_context) {
        eprintln!("Warning: could not record the audit event: {error:#}");
        audit = None;
    }
    let result = tools::execute_shell(&session_root, &cwd, command, None).await;
    if audit.is_some() {
        let success = result.is_ok();
        let output = result
            .as_ref()
            .map_or_else(|error| format!("{error:#}"), Clone::clone);
        let failure = result.as_ref().err().map(tools::audit_failure);
        let recorded = control_plane::tool_finished(
            &mut audit,
            call_id,
            "exec",
            success,
            &output,
            failure.as_ref(),
        )
        .unwrap_or(Ok(()));
        let finalized = recorded.and_then(|()| {
            audit
                .take()
                .map(|session| control_plane::finish_session(session, success))
                .transpose()
                .map(|_| ())
        });
        if let Err(error) = finalized {
            eprintln!("Warning: could not finalize the audit session: {error:#}");
        }
    }
    result
}

pub async fn run_session_worker(id: String, workspace: std::path::PathBuf) -> Result<()> {
    sessions::run_worker(id, workspace).await
}

pub async fn run_tui() -> Result<()> {
    let cwd = std::env::current_dir().context("could not determine the working directory")?;
    let warmup_workspace = cwd.clone();
    tokio::spawn(async move {
        let _ = tools::prewarm(&warmup_workspace).await;
    });
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
    let _ = execute!(
        std::io::stdout(),
        Clear(ClearType::Purge),
        Clear(ClearType::All),
        MoveTo(0, 0)
    );
    result.context("terminal error")?;
    Ok(())
}
