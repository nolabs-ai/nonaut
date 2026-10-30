use std::{
    env, fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::{
    cursor::{MoveTo, Show},
    event::{DisableMouseCapture, PopKeyboardEnhancementFlags},
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::mpsc,
    time::{MissedTickBehavior, interval, sleep},
};

use crate::cli::{self, ACCENT, MUTED, SUCCESS, styled};

const DETACHED_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Control {
    Attach { columns: u16, rows: u16 },
    Detach,
    Kill,
    Ping,
    Resize { columns: u16, rows: u16 },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Metadata {
    id: String,
    pid: u32,
    workspace: PathBuf,
    created_at: u64,
    detached_at: u64,
    expires_at: u64,
    #[serde(default)]
    attached: bool,
}

struct Paths {
    state: PathBuf,
    runtime: PathBuf,
}

impl Paths {
    fn discover() -> Result<Self> {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set; cannot determine XDG session directories")?;
        let state_home = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"));
        let state = state_home.join("nonaut/sessions");
        let runtime = match env::var_os("XDG_RUNTIME_DIR") {
            Some(directory) => PathBuf::from(directory).join("nonaut"),
            None => state_home.join("nonaut/runtime"),
        };
        create_private_dir(&state)?;
        create_private_dir(&runtime)?;
        Ok(Self { state, runtime })
    }

    fn metadata(&self, id: &str) -> PathBuf {
        self.state.join(format!("{id}.json"))
    }

    fn socket(&self, id: &str) -> PathBuf {
        self.runtime.join(format!("{id}.sock"))
    }
}

pub(crate) async fn run_cli(args: Vec<String>, requested_workspace: Option<&str>) -> Result<()> {
    match args.as_slice() {
        [] => start(requested_workspace).await,
        [command] if command == "ps" => list().await,
        [command, flag] if command == "ps" && matches!(flag.as_str(), "--help" | "-h") => {
            cli::print_command_help("ps", "nonaut ps", "List sessions kept alive with /detach.");
            Ok(())
        }
        [command, flag] if command == "attach" && matches!(flag.as_str(), "--help" | "-h") => {
            cli::print_command_help(
                "attach",
                "nonaut attach <session>",
                "Attach to a persistent Nonaut session.",
            );
            Ok(())
        }
        [command, id] if command == "attach" => attach_existing(id).await,
        [command, flag] if command == "kill" && matches!(flag.as_str(), "--help" | "-h") => {
            cli::print_command_help(
                "kill",
                "nonaut kill <session> | --all | -a",
                "Stop one or all persistent Nonaut sessions.",
            );
            Ok(())
        }
        [command, flag] if command == "kill" && matches!(flag.as_str(), "--all" | "-a") => {
            kill_all().await
        }
        [command, id] if command == "kill" => kill_one(id).await,
        [command, ..] => {
            bail!("unknown command or arguments `{command}`\n\nRun `nonaut help` for usage.")
        }
    }
}

async fn start(requested_workspace: Option<&str>) -> Result<()> {
    let launch = env::current_dir().context("could not determine the working directory")?;
    let selected = crate::workspace::select(&launch, requested_workspace)?;
    let workspace = if requested_workspace.is_none() && !selected.configured {
        crate::workspace_setup::prompt(&launch)?.root
    } else {
        selected.root
    };
    let id = format!("{}-{}", unix_seconds(), std::process::id());
    let paths = Paths::discover()?;
    let socket = paths.socket(&id);
    let executable = env::current_exe().context("could not locate the nonaut executable")?;
    let mut command = Command::new(executable);
    command
        .arg("__nonaut_session")
        .arg(&id)
        .arg(&workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command
        .spawn()
        .context("could not start the nonaut session worker")?;
    wait_for_socket(&socket).await?;
    proxy(&id, &socket).await
}

async fn attach_existing(id: &str) -> Result<()> {
    validate_id(id)?;
    let paths = Paths::discover()?;
    let metadata = read_metadata(&paths.metadata(id))
        .with_context(|| format!("detached session `{id}` was not found"))?;
    if metadata.expires_at <= unix_seconds() {
        let _ = send_control(&paths.socket(id), &Control::Kill).await;
        cleanup(&paths, id);
        bail!("session `{id}` has expired");
    }
    proxy(id, &paths.socket(id)).await
}

async fn list() -> Result<()> {
    let paths = Paths::discover()?;
    let sessions = live_sessions(&paths).await?;
    let color = cli::color_enabled();
    let width = crossterm::terminal::size()
        .map(|(columns, _)| usize::from(columns))
        .unwrap_or(120);
    print!("{}", render_session_list(&sessions, color, width));
    Ok(())
}

fn render_session_list(sessions: &[Metadata], color: bool, terminal_width: usize) -> String {
    let title = styled("⎔  nonaut sessions", ACCENT, true, color);
    if sessions.is_empty() {
        return format!(
            "{title}\n\n{}\n{}\n",
            styled("No persistent sessions.", MUTED, false, color),
            styled(
                "Use /detach inside the TUI to keep one running.",
                MUTED,
                false,
                color
            )
        );
    }

    let session_width = sessions
        .iter()
        .map(|session| session.id.chars().count())
        .max()
        .unwrap_or(7)
        .max(7);
    let pid_width = sessions
        .iter()
        .map(|session| session.pid.to_string().len())
        .max()
        .unwrap_or(3)
        .max(3);
    let status_width = 10;
    let expires_width = 8;
    let separators = 8;
    let fixed_width = session_width + status_width + pid_width + expires_width + separators;
    let workspace_width = terminal_width.saturating_sub(fixed_width).max(16);

    let mut output = format!(
        "{title}\n{}\n\n",
        styled(
            &format!(
                "{} persistent session{}",
                sessions.len(),
                if sessions.len() == 1 { "" } else { "s" }
            ),
            MUTED,
            false,
            color
        )
    );
    output.push_str(&format!(
        "{}  {}  {}  {}  {}\n",
        styled(&pad("SESSION", session_width), MUTED, true, color),
        styled(&pad("STATUS", status_width), MUTED, true, color),
        styled(&pad("PID", pid_width), MUTED, true, color),
        styled(&pad("EXPIRES", expires_width), MUTED, true, color),
        styled("WORKSPACE", MUTED, true, color)
    ));

    for session in sessions {
        let (status, status_color) = if session.attached {
            ("◆ attached", SUCCESS)
        } else {
            ("◇ detached", ACCENT)
        };
        output.push_str(&format!(
            "{}  {}  {}  {}  {}\n",
            styled(&pad(&session.id, session_width), ACCENT, false, color),
            styled(&pad(status, status_width), status_color, false, color),
            styled(
                &pad(&session.pid.to_string(), pid_width),
                MUTED,
                false,
                color
            ),
            styled(
                &pad(&format_remaining(session.expires_at), expires_width),
                MUTED,
                false,
                color
            ),
            shorten(&display_workspace(&session.workspace), workspace_width)
        ));
    }
    output
}

fn pad(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(value.chars().count());
    format!("{value}{}", " ".repeat(padding))
}

fn display_workspace(workspace: &Path) -> String {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return workspace.display().to_string();
    };
    match workspace.strip_prefix(home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => workspace.display().to_string(),
    }
}

fn shorten(value: &str, width: usize) -> String {
    let count = value.chars().count();
    if count <= width {
        return value.to_owned();
    }
    if width <= 1 {
        return "…".to_owned();
    }
    let tail = value.chars().rev().take(width - 1).collect::<Vec<_>>();
    format!("…{}", tail.into_iter().rev().collect::<String>())
}

async fn kill_one(id: &str) -> Result<()> {
    validate_id(id)?;
    let paths = Paths::discover()?;
    if !paths.metadata(id).exists() {
        bail!("persistent session `{id}` was not found");
    }
    let _ = send_control(&paths.socket(id), &Control::Kill).await;
    cleanup(&paths, id);
    cli::success(&format!("Killed session {id}."));
    Ok(())
}

async fn kill_all() -> Result<()> {
    let paths = Paths::discover()?;
    let sessions = metadata_files(&paths)?;
    for metadata in &sessions {
        let _ = send_control(&paths.socket(&metadata.id), &Control::Kill).await;
        cleanup(&paths, &metadata.id);
    }
    if sessions.is_empty() {
        cli::notice("No persistent sessions to kill.", None);
    } else {
        cli::success(&format!(
            "Killed {} persistent session{}.",
            sessions.len(),
            if sessions.len() == 1 { "" } else { "s" }
        ));
    }
    Ok(())
}

pub(crate) async fn run_worker(id: String, workspace: PathBuf) -> Result<()> {
    validate_id(&id)?;
    let paths = Paths::discover()?;
    let socket_path = paths.socket(&id);
    let _ = fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("could not bind {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

    let (master, writer, mut output, mut exited, mut killer) = spawn_tui(&workspace, &socket_path)?;
    let mut attached: Option<mpsc::UnboundedSender<Vec<u8>>> = None;
    let mut attached_once = false;
    let mut startup_output = Vec::new();
    let mut persistent = false;
    let created_at = unix_seconds();
    let mut expires_at = created_at + DETACHED_TTL.as_secs();
    let (gone_tx, mut gone_rx) = mpsc::unbounded_channel();
    let mut expiry = interval(Duration::from_secs(30));
    expiry.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = accepted.context("session control socket failed")?;
                let control = read_control(&mut stream).await;
                match control {
                    Ok(Control::Attach { columns, rows }) => {
                        if attached.is_some() {
                            let _ = stream
                                .write_all(b"ERR session is already attached\n")
                                .await;
                            continue;
                        }
                        force_resize(master.as_ref(), columns, rows)?;
                        if stream.write_all(b"OK\n").await.is_err() {
                            continue;
                        }
                        let (sender, receiver) = mpsc::unbounded_channel();
                        if !startup_output.is_empty() {
                            let _ = sender.send(std::mem::take(&mut startup_output));
                        }
                        attached = Some(sender);
                        attached_once = true;
                        if persistent {
                            expires_at = unix_seconds() + DETACHED_TTL.as_secs();
                            update_metadata(&paths.metadata(&id), expires_at, true)?;
                        }
                        let writer = Arc::clone(&writer);
                        let gone = gone_tx.clone();
                        tokio::spawn(proxy_connection(stream, receiver, Arc::clone(&writer), gone));
                        tokio::spawn(async move {
                            sleep(Duration::from_millis(100)).await;
                            if let Ok(mut pty) = writer.lock() {
                                let _ = pty.write_all(&[0x0c]);
                            }
                        });
                    }
                    Ok(Control::Detach) => {
                        persistent = true;
                        expires_at = unix_seconds() + DETACHED_TTL.as_secs();
                        write_metadata(&paths.metadata(&id), &Metadata {
                            id: id.clone(),
                            pid: std::process::id(),
                            workspace: workspace.clone(),
                            created_at,
                            detached_at: unix_seconds(),
                            expires_at,
                            attached: false,
                        })?;
                        attached = None;
                        let _ = stream.write_all(b"OK\n").await;
                    }
                    Ok(Control::Kill) => {
                        let _ = stream.write_all(b"OK\n").await;
                        let _ = killer.kill();
                        break;
                    }
                    Ok(Control::Ping) => {
                        let _ = stream.write_all(b"OK\n").await;
                    }
                    Ok(Control::Resize { columns, rows }) => {
                        master.resize(PtySize { rows: rows.max(1), cols: columns.max(1), pixel_width: 0, pixel_height: 0 })?;
                        let _ = stream.write_all(b"OK\n").await;
                    }
                    Err(error) => {
                        let _ = stream.write_all(format!("ERR {error}\n").as_bytes()).await;
                    }
                }
            }
            Some(bytes) = output.recv() => {
                if let Some(sender) = &attached
                    && sender.send(bytes.clone()).is_err()
                {
                    attached = None;
                } else if attached.is_none() && !attached_once && startup_output.len() < 1024 * 1024 {
                    startup_output.extend_from_slice(&bytes);
                }
            }
            Some(()) = gone_rx.recv() => {
                attached = None;
                if attached_once && !persistent {
                    let _ = killer.kill();
                    break;
                }
                if persistent {
                    expires_at = unix_seconds() + DETACHED_TTL.as_secs();
                    update_metadata(&paths.metadata(&id), expires_at, false)?;
                }
            }
            _ = &mut exited => break,
            _ = expiry.tick() => {
                if persistent && attached.is_none() && unix_seconds() >= expires_at {
                    let _ = killer.kill();
                    break;
                }
            }
        }
    }
    cleanup(&paths, &id);
    Ok(())
}

type PtyParts = (
    Box<dyn MasterPty + Send>,
    Arc<Mutex<Box<dyn Write + Send>>>,
    mpsc::UnboundedReceiver<Vec<u8>>,
    tokio::sync::oneshot::Receiver<()>,
    Box<dyn portable_pty::ChildKiller + Send + Sync>,
);

fn spawn_tui(workspace: &Path, socket: &Path) -> Result<PtyParts> {
    let pair = native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let executable = env::current_exe().context("could not locate the nonaut executable")?;
    let mut command = CommandBuilder::new(executable);
    command.arg("__nonaut_tui");
    command.cwd(workspace);
    command.env("NONAUT_SESSION_SOCKET", socket);
    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);
    let killer = child.clone_killer();
    let mut reader = pair.master.try_clone_reader()?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
    let (output_tx, output_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut buffer = vec![0_u8; 16 * 1024];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 || output_tx.send(buffer[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = child.wait();
        let _ = exit_tx.send(());
    });
    Ok((pair.master, writer, output_rx, exit_rx, killer))
}

async fn proxy_connection(
    stream: UnixStream,
    mut output: mpsc::UnboundedReceiver<Vec<u8>>,
    pty: Arc<Mutex<Box<dyn Write + Send>>>,
    gone: mpsc::UnboundedSender<()>,
) {
    let (mut reader, mut writer) = stream.into_split();
    let mut input = vec![0_u8; 8192];
    loop {
        tokio::select! {
            received = output.recv() => match received {
                Some(bytes) if writer.write_all(&bytes).await.is_ok() => {}
                _ => break,
            },
            received = reader.read(&mut input) => match received {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let result = pty.lock().map_err(|_| ()).and_then(|mut writer| writer.write_all(&input[..count]).map_err(|_| ()));
                    if result.is_err() { break; }
                }
            }
        }
    }
    let _ = gone.send(());
}

async fn proxy(id: &str, socket: &Path) -> Result<()> {
    let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("could not connect to session `{id}`"))?;
    write_control(&mut stream, &Control::Attach { columns, rows }).await?;
    let response = read_line(&mut stream).await?;
    if response != "OK" {
        bail!("{}", response.strip_prefix("ERR ").unwrap_or(&response));
    }

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let result = proxy_terminal(stream, socket).await;
    restore_terminal();

    let paths = Paths::discover()?;
    if paths.metadata(id).exists() {
        cli::notice(
            &format!("Session {id} detached."),
            Some(&format!("Reattach with: nonaut attach {id}")),
        );
    }
    result
}

fn restore_terminal() {
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        Clear(ClearType::Purge),
        Clear(ClearType::All),
        MoveTo(0, 0),
        Show
    );
    let _ = disable_raw_mode();
}

async fn proxy_terminal(stream: UnixStream, socket: &Path) -> Result<()> {
    let (mut socket_reader, mut socket_writer) = stream.into_split();
    let mut terminal_input = terminal_input();
    let mut stdout = tokio::io::stdout();
    let mut from_session = vec![0_u8; 16 * 1024];
    let mut size = crossterm::terminal::size().unwrap_or((80, 24));
    let mut resize = interval(Duration::from_millis(500));
    resize.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            input = terminal_input.recv() => {
                match input {
                    Some(Ok(bytes)) => socket_writer.write_all(&bytes).await?,
                    Some(Err(error)) => return Err(error).context("could not read terminal input"),
                    None => break,
                }
            }
            read = socket_reader.read(&mut from_session) => {
                let count = read?;
                if count == 0 { break; }
                stdout.write_all(&from_session[..count]).await?;
                stdout.flush().await?;
            }
            _ = resize.tick() => {
                let next = crossterm::terminal::size().unwrap_or(size);
                if next != size {
                    size = next;
                    let _ = send_control(socket, &Control::Resize { columns: size.0, rows: size.1 }).await;
                }
            }
        }
    }
    Ok(())
}

fn terminal_input() -> mpsc::UnboundedReceiver<std::io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = vec![0_u8; 8192];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender.send(Ok(buffer[..count].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });
    receiver
}

pub(crate) fn request_detach() -> Result<()> {
    let socket = env::var_os("NONAUT_SESSION_SOCKET")
        .map(PathBuf::from)
        .context("this session is not managed by a nonaut worker")?;
    let mut stream = std::os::unix::net::UnixStream::connect(&socket)
        .with_context(|| format!("could not reach {}", socket.display()))?;
    let encoded = serde_json::to_vec(&Control::Detach)?;
    stream.write_all(&encoded)?;
    stream.write_all(b"\n")?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    if !response.starts_with("OK") {
        bail!("session worker rejected detach: {}", response.trim());
    }
    Ok(())
}

async fn live_sessions(paths: &Paths) -> Result<Vec<Metadata>> {
    let now = unix_seconds();
    let mut live = Vec::new();
    for metadata in metadata_files(paths)? {
        if metadata.expires_at <= now
            || send_control(&paths.socket(&metadata.id), &Control::Ping)
                .await
                .is_err()
        {
            let _ = send_control(&paths.socket(&metadata.id), &Control::Kill).await;
            cleanup(paths, &metadata.id);
        } else {
            live.push(metadata);
        }
    }
    live.sort_by_key(|metadata| metadata.created_at);
    Ok(live)
}

fn metadata_files(paths: &Paths) -> Result<Vec<Metadata>> {
    let mut sessions = Vec::new();
    for entry in fs::read_dir(&paths.state)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if let Ok(metadata) = read_metadata(&path) {
            sessions.push(metadata);
        }
    }
    Ok(sessions)
}

async fn send_control(socket: &Path, control: &Control) -> Result<()> {
    let mut stream = UnixStream::connect(socket).await?;
    write_control(&mut stream, control).await?;
    let response = read_line(&mut stream).await?;
    if response == "OK" {
        Ok(())
    } else {
        Err(anyhow!(response))
    }
}

async fn write_control(stream: &mut UnixStream, control: &Control) -> Result<()> {
    stream.write_all(&serde_json::to_vec(control)?).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

async fn read_control(stream: &mut UnixStream) -> Result<Control> {
    let line = read_line(stream).await?;
    serde_json::from_str(&line).context("invalid session control request")
}

async fn read_line(stream: &mut UnixStream) -> Result<String> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if stream.read(&mut byte).await? == 0 {
            bail!("session closed during handshake");
        }
        if byte[0] == b'\n' {
            break;
        }
        if line.len() >= 4096 {
            bail!("session handshake was too large");
        }
        line.push(byte[0]);
    }
    String::from_utf8(line).context("session handshake was not UTF-8")
}

async fn wait_for_socket(socket: &Path) -> Result<()> {
    let started = std::time::Instant::now();
    while started.elapsed() < STARTUP_TIMEOUT {
        if socket.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(25)).await;
    }
    bail!("session worker did not start within five seconds")
}

fn write_metadata(path: &Path, metadata: &Metadata) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(metadata)?)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn update_metadata(path: &Path, expires_at: u64, attached: bool) -> Result<()> {
    let mut metadata = read_metadata(path)?;
    metadata.expires_at = expires_at;
    metadata.attached = attached;
    write_metadata(path, &metadata)
}

fn force_resize(master: &dyn MasterPty, columns: u16, rows: u16) -> Result<()> {
    let columns = columns.max(1);
    let rows = rows.max(1);
    let nudge_rows = if rows == 1 { 2 } else { rows - 1 };
    master.resize(PtySize {
        rows: nudge_rows,
        cols: columns,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    master.resize(PtySize {
        rows,
        cols: columns,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    Ok(())
}

fn read_metadata(path: &Path) -> Result<Metadata> {
    serde_json::from_slice(&fs::read(path)?).context("invalid session metadata")
}

fn cleanup(paths: &Paths, id: &str) {
    let _ = fs::remove_file(paths.metadata(id));
    let _ = fs::remove_file(paths.socket(id));
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn format_remaining(expires_at: u64) -> String {
    let remaining = expires_at.saturating_sub(unix_seconds());
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    format!("{hours}h {minutes}m")
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-') {
        bail!("invalid session id `{id}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_protocol_round_trips() {
        let encoded = serde_json::to_string(&Control::Attach {
            columns: 120,
            rows: 40,
        })
        .unwrap();
        let decoded: Control = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded,
            Control::Attach {
                columns: 120,
                rows: 40
            }
        ));
    }

    #[test]
    fn remaining_time_is_human_readable() {
        let value = format_remaining(unix_seconds() + 3660);
        assert!(value.starts_with("1h "));
    }

    #[test]
    fn session_ids_cannot_escape_the_xdg_directory() {
        assert!(validate_id("1792348665-92440").is_ok());
        assert!(validate_id("../../metadata").is_err());
        assert!(validate_id("").is_err());
    }

    #[test]
    fn session_list_uses_aligned_plain_text_when_piped() {
        let sessions = vec![Metadata {
            id: "1792348665-92440".to_owned(),
            pid: 92448,
            workspace: PathBuf::from("/tmp/nonaut"),
            created_at: unix_seconds(),
            detached_at: unix_seconds(),
            expires_at: unix_seconds() + 3600,
            attached: false,
        }];
        let output = render_session_list(&sessions, false, 100);
        assert!(output.contains("⎔  nonaut sessions"));
        assert!(output.contains("◇ detached"));
        assert!(output.contains("SESSION"));
        assert!(output.contains("STATUS"));
        assert!(!output.contains('\t'));
        assert!(!output.contains("\u{1b}["));
    }

    #[test]
    fn long_workspaces_are_shortened_from_the_left() {
        assert_eq!(shorten("~/Work/a/very/long/project", 12), "…ong/project");
    }
}
