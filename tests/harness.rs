#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

struct TestRoot {
    path: PathBuf,
    runtime: PathBuf,
}

impl TestRoot {
    fn new(name: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/nonaut-integration")
            .join(format!("{name}-{}-{sequence}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        let runtime =
            std::env::temp_dir().join(format!("nonaut-it-{}-{sequence}", std::process::id()));
        fs::create_dir_all(&runtime).unwrap();
        Self { path, runtime }
    }

    fn workspace(&self) -> PathBuf {
        let path = self.path.join("workspace");
        fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        let _ = fs::remove_dir_all(&self.runtime);
    }
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_nonaut")
}

fn exec(workspace: &Path, command: &str) -> Output {
    exec_in(workspace, workspace, command)
}

fn exec_in(workspace: &Path, cwd: &Path, command: &str) -> Output {
    // A private config dir keeps the developer's own config.json out of tests.
    let home = workspace.parent().unwrap();
    let config = home.join("config");
    fs::create_dir_all(config_dir(workspace)).unwrap();
    Command::new(binary())
        .arg("exec")
        .arg("--cwd")
        .arg(cwd)
        .arg("--")
        .arg(command)
        .current_dir(workspace)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", &config)
        .env("NONAUT_TEST_SECRET", "must-not-cross-the-boundary")
        .output()
        .unwrap()
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn workspace_rules_follow_command_cwd_and_rust_profile() {
    use std::os::unix::fs::PermissionsExt;

    let root = TestRoot::new("workspace-policy");
    let session = root.workspace();
    let rust = session.join("nono");
    let other = session.join("other");
    fs::create_dir_all(&rust).unwrap();
    fs::create_dir_all(&other).unwrap();

    let cargo_bin = root.path.join(".cargo/bin");
    fs::create_dir_all(&cargo_bin).unwrap();
    fs::create_dir_all(root.path.join(".rustup")).unwrap();
    let cargo = cargo_bin.join("cargo");
    fs::write(&cargo, "#!/bin/sh\nprintf rust-profile-ok\n").unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();

    let config = serde_json::json!({
        "sandbox": {
            "unconfigured_workspaces": "deny",
            "workspaces": {
                (rust.to_string_lossy().to_string()): {"profile": "rust-dev"}
            }
        }
    });
    write_config(&session, &serde_json::to_string(&config).unwrap());

    let allowed = exec_in(&session, &rust, "cargo --version");
    assert!(allowed.status.success(), "{}", combined(&allowed));
    assert!(combined(&allowed).contains("rust-profile-ok"));

    let denied = exec_in(&session, &other, "printf should-not-run");
    assert!(
        !denied.status.success(),
        "unconfigured workspace was allowed"
    );
    assert!(combined(&denied).contains("outside every configured workspace"));
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn headless_exec_runs_and_writes_inside_the_workspace() {
    let root = TestRoot::new("exec-success");
    let workspace = root.workspace();

    let output = exec(&workspace, "printf 'hello from nono'");
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "hello from nono"
    );

    let output = exec(&workspace, "printf 'sandboxed write' > created.txt");
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(
        fs::read_to_string(workspace.join("created.txt")).unwrap(),
        "sandboxed write"
    );
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn headless_exec_blocks_outside_files_and_inherited_secrets() {
    let root = TestRoot::new("exec-policy");
    let workspace = root.workspace();
    let outside = root.path.join("outside.txt");
    fs::write(&outside, "private sentinel").unwrap();

    let output = exec(&workspace, &format!("cat '{}'", outside.display()));
    assert!(
        !output.status.success(),
        "outside read unexpectedly succeeded"
    );
    let failure = combined(&output);
    assert!(
        failure.contains("nono sandbox") || failure.contains("Operation not permitted"),
        "unexpected denial output: {failure}"
    );

    let output = exec(
        &workspace,
        "if [ -z \"$NONAUT_TEST_SECRET\" ]; then printf hidden; else printf leaked; fi",
    );
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hidden");
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn headless_exec_blocks_network_connections() {
    if !curl_available() {
        return;
    }
    let root = TestRoot::new("exec-network");
    let workspace = root.workspace();
    write_config(&workspace, r#"{"sandbox":{"network":{"mode":"blocked"}}}"#);
    assert!(
        !curl_reaches_listener(&workspace),
        "network access unexpectedly succeeded with \"network\": \"blocked\""
    );
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn headless_exec_allows_network_when_explicitly_opened() {
    if !curl_available() {
        return;
    }
    let root = TestRoot::new("exec-network-open");
    let workspace = root.workspace();
    write_config(&workspace, r#"{"sandbox":{"network":{"mode":"open"}}}"#);
    assert!(
        curl_reaches_listener(&workspace),
        "sandboxed command could not reach the listener with explicitly open network"
    );
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn command_cwd_selects_the_workspace_network_policy() {
    if !curl_available() {
        return;
    }
    let root = TestRoot::new("workspace-network");
    let session = root.workspace();
    let open = session.join("open");
    let blocked = session.join("blocked");
    fs::create_dir_all(&open).unwrap();
    fs::create_dir_all(&blocked).unwrap();
    let config = serde_json::json!({
        "sandbox": {
            "unconfigured_workspaces": "deny",
            "workspaces": {
                (open.to_string_lossy().to_string()): {
                    "network": {"mode": "open"}
                },
                (blocked.to_string_lossy().to_string()): {
                    "network": {"mode": "blocked"}
                }
            }
        }
    });
    write_config(&session, &serde_json::to_string(&config).unwrap());
    assert!(curl_reaches_listener_in(&session, &open));
    assert!(!curl_reaches_listener_in(&session, &blocked));
}

fn write_config(workspace: &Path, config_text: &str) {
    let config = config_dir(workspace);
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.json"), config_text).unwrap();
}

fn config_dir(workspace: &Path) -> PathBuf {
    let home = workspace.parent().unwrap();
    home.join("config/nonaut")
}

fn curl_available() -> bool {
    Command::new("sh")
        .args(["-c", "command -v curl"])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Runs a sandboxed curl against a local listener; true if it got through.
fn curl_reaches_listener(workspace: &Path) -> bool {
    curl_reaches_listener_in(workspace, workspace)
}

fn curl_reaches_listener_in(workspace: &Path, cwd: &Path) -> bool {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nallowed",
                    );
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return false,
            }
        }
        false
    });
    let output = exec_in(
        workspace,
        cwd,
        &format!("curl --silent --show-error --max-time 2 http://{address}"),
    );
    let reached = server.join().unwrap();
    assert_eq!(
        output.status.success(),
        reached,
        "curl result and listener disagree: {}",
        combined(&output)
    );
    reached
}

struct PtyRun {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
}

fn spawn_pty(workspace: &Path, arguments: &[&str], root: &TestRoot) -> PtyRun {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 32,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(binary());
    for argument in arguments {
        command.arg(argument);
    }
    command.cwd(workspace);
    command.env("NONAUT_NO_AUTO_CONNECT", "1");
    command.env("HOME", &root.path);
    command.env("XDG_CONFIG_HOME", root.path.join("config"));
    command.env("XDG_STATE_HOME", root.path.join("state"));
    command.env("XDG_RUNTIME_DIR", &root.runtime);
    command.env("TERM", "xterm-256color");
    let child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&output);
    thread::spawn(move || {
        let mut buffer = [0; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            captured.lock().unwrap().extend_from_slice(&buffer[..count]);
        }
    });
    PtyRun {
        child,
        writer,
        output,
    }
}

fn wait_for(output: &Arc<Mutex<Vec<u8>>>, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let visible = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
        if visible.contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "timed out waiting for {needle:?}\n{}",
        String::from_utf8_lossy(&output.lock().unwrap())
    );
}

fn wait_for_exit(child: &mut Box<dyn portable_pty::Child + Send + Sync>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    panic!("PTY child did not exit");
}

fn answer_cursor_position(run: &mut PtyRun) {
    wait_for(&run.output, "\x1b[6n");
    run.writer.write_all(b"\x1b[1;1R").unwrap();
    run.writer.flush().unwrap();
}

#[test]
#[ignore = "provider-free PTY integration test"]
fn tui_exec_interrupts_and_restores_terminal_modes() {
    let root = TestRoot::new("tui-exec");
    let workspace = root.workspace();
    let mut run = spawn_pty(&workspace, &["__nonaut_tui"], &root);
    wait_for(&run.output, "Ask anything");
    run.writer.write_all(b"/exec sleep 30\r").unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "Running command in nono");
    run.writer.write_all(b"\x1b").unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "Command interrupted");
    run.writer.write_all(b"/q\r").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);

    let output = run.output.lock().unwrap();
    assert!(output.windows(8).any(|window| window == b"\x1b[?1000l"));
    assert!(output.windows(8).any(|window| window == b"\x1b[?1049l"));
    let config = root.path.join("config/nonaut");
    assert!(config.join("config.json").is_file());
    assert_eq!(fs::read_dir(config).unwrap().count(), 1);
}

#[test]
#[ignore = "provider-free PTY integration test"]
fn tui_changes_view_shows_untracked_work_and_returns_to_agent() {
    let root = TestRoot::new("tui-changes");
    let workspace = root.workspace();
    let initialized = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .current_dir(&workspace)
        .status()
        .unwrap();
    assert!(initialized.success());
    fs::write(
        workspace.join("review-me.txt"),
        "visible in the changes workspace\n",
    )
    .unwrap();

    let mut run = spawn_pty(&workspace, &["__nonaut_tui"], &root);
    wait_for(&run.output, "2. Review (1)");
    run.writer.write_all(b"\x1b2").unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "visible in the changes workspace");
    run.writer.write_all(b"\x1b1/q\r").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);
}

#[test]
#[ignore = "provider-free PTY integration test"]
fn detached_session_can_be_listed_attached_and_killed() {
    let root = TestRoot::new("session-lifecycle");
    let workspace = root.workspace();
    let mut run = spawn_pty(&workspace, &[], &root);
    answer_cursor_position(&mut run);
    wait_for(&run.output, "Ask anything");
    run.writer.write_all(b"/detach\r").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);

    let environment = |command: &mut Command| {
        command
            .current_dir(&workspace)
            .env("NONAUT_NO_AUTO_CONNECT", "1")
            .env("XDG_STATE_HOME", root.path.join("state"))
            .env("XDG_RUNTIME_DIR", &root.runtime);
    };
    let mut ps = Command::new(binary());
    ps.arg("ps");
    environment(&mut ps);
    let listed = ps.output().unwrap();
    assert!(listed.status.success(), "{}", combined(&listed));
    let listing = String::from_utf8_lossy(&listed.stdout);
    assert!(listing.contains("1 persistent session"), "{listing}");
    let id = listing
        .lines()
        .find_map(|line| {
            line.split_whitespace()
                .next()
                .filter(|value| value.contains('-'))
        })
        .unwrap()
        .to_owned();

    let mut attached = spawn_pty(&workspace, &["attach", &id], &root);
    answer_cursor_position(&mut attached);
    wait_for(&attached.output, "Ask anything");
    attached.writer.write_all(b"/detach\r").unwrap();
    attached.writer.flush().unwrap();
    wait_for_exit(&mut attached.child);

    let mut kill = Command::new(binary());
    kill.args(["kill", "--all"]);
    environment(&mut kill);
    let killed = kill.output().unwrap();
    assert!(killed.status.success(), "{}", combined(&killed));
    assert!(String::from_utf8_lossy(&killed.stdout).contains("Killed 1 persistent session"));
}
