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
    exec_in_selected(workspace, cwd, command, None)
}

fn exec_in_selected(
    workspace: &Path,
    cwd: &Path,
    command: &str,
    selected_workspace: Option<&str>,
) -> Output {
    // A private config dir keeps the developer's own config.json out of tests.
    let home = workspace.parent().unwrap();
    let config = home.join("config");
    fs::create_dir_all(config_dir(workspace)).unwrap();
    let mut process = Command::new(binary());
    if let Some(name) = selected_workspace {
        process.args(["--workspace", name]);
    }
    process
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
fn workspace_rules_follow_command_cwd_and_combine_development_profiles() {
    use std::os::unix::fs::PermissionsExt;

    let root = TestRoot::new("workspace-policy");
    let session = root.workspace();
    let mixed = session.join("nono");
    let other = session.join("other");
    fs::create_dir_all(&mixed).unwrap();
    fs::create_dir_all(&other).unwrap();

    let cargo_bin = root.path.join(".cargo/bin");
    fs::create_dir_all(&cargo_bin).unwrap();
    fs::create_dir_all(root.path.join(".rustup")).unwrap();
    let cargo = cargo_bin.join("cargo");
    fs::write(&cargo, "#!/bin/sh\nprintf rust-profile-ok\n").unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();

    let python_bin = root.path.join(".pyenv/shims");
    fs::create_dir_all(&python_bin).unwrap();
    let python_executable = python_bin.join("python");
    fs::write(
        &python_executable,
        "#!/bin/sh\ntest -n \"$PIP_CACHE_DIR\" && printf python-profile-ok > \"$PIP_CACHE_DIR/probe\" && printf python-profile-ok\n",
    )
    .unwrap();
    fs::set_permissions(&python_executable, fs::Permissions::from_mode(0o755)).unwrap();

    let node_bin = root.path.join(".nvm/versions/node/v24.0.0/bin");
    fs::create_dir_all(&node_bin).unwrap();
    let node = node_bin.join("node");
    fs::write(
        &node,
        "#!/bin/sh\ntest -n \"$npm_config_cache\" && printf node-profile-ok > \"$npm_config_cache/probe\" && printf node-profile-ok\n",
    )
    .unwrap();
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();

    let go_bin = root.path.join("go/bin");
    fs::create_dir_all(&go_bin).unwrap();
    let go = go_bin.join("go");
    fs::write(
        &go,
        "#!/bin/sh\ntest -n \"$GOCACHE\" && printf go-profile-ok > \"$GOCACHE/probe\" && printf go-profile-ok\n",
    )
    .unwrap();
    fs::set_permissions(&go, fs::Permissions::from_mode(0o755)).unwrap();

    let bun_bin = root.path.join(".bun/bin");
    fs::create_dir_all(&bun_bin).unwrap();
    let bun = bun_bin.join("bun");
    fs::write(
        &bun,
        "#!/bin/sh\ntest -n \"$BUN_INSTALL_CACHE_DIR\" && printf bun-profile-ok > \"$BUN_INSTALL_CACHE_DIR/probe\" && printf bun-profile-ok\n",
    )
    .unwrap();
    fs::set_permissions(&bun, fs::Permissions::from_mode(0o755)).unwrap();

    let config = serde_json::json!({
        "workspace": {
            "default": "mixed",
            "definitions": {
                "mixed": {
                    "path": mixed,
                    "profiles": ["rust", "python", "node", "go", "bun"]
                }
            }
        }
    });
    write_config(&session, &serde_json::to_string(&config).unwrap());

    let allowed = exec_in(
        &session,
        &mixed,
        "cargo --version && printf ':' && python --version && printf ':' && node --version && printf ':' && go version && printf ':' && bun --version",
    );
    assert!(allowed.status.success(), "{}", combined(&allowed));
    assert!(combined(&allowed).contains("rust-profile-ok"));
    assert!(combined(&allowed).contains("python-profile-ok"));
    assert!(combined(&allowed).contains("node-profile-ok"));
    assert!(combined(&allowed).contains("go-profile-ok"));
    assert!(combined(&allowed).contains("bun-profile-ok"));

    let denied = exec_in(&session, &other, "printf should-not-run");
    assert!(
        !denied.status.success(),
        "unconfigured workspace was allowed"
    );
    assert!(combined(&denied).contains("outside workspace"));
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
fn headless_exec_uses_a_private_writable_home_outside_the_workspace() {
    let root = TestRoot::new("exec-private-home");
    let workspace = root.workspace();
    let output = exec(
        &workspace,
        r#"test "$HOME" != "$PWD" && printf private > "$HOME/state" && test ! -e state"#,
    );
    assert!(output.status.success(), "{}", combined(&output));
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
fn sandboxed_commands_can_read_but_not_modify_the_capability_manifest() {
    let root = TestRoot::new("capability-manifest");
    let workspace = root.workspace();

    let readable = exec(
        &workspace,
        "test -n \"$NONAUT_CAP_FILE\" && test -r \"$NONAUT_CAP_FILE\" && cat \"$NONAUT_CAP_FILE\"",
    );
    assert!(readable.status.success(), "{}", combined(&readable));
    let manifest: serde_json::Value =
        serde_json::from_slice(&readable.stdout).expect("capability file was not JSON");
    assert_eq!(manifest["enforced_by"], "nono");
    assert_eq!(
        manifest["workspace"],
        serde_json::json!(workspace.canonicalize().unwrap())
    );
    assert!(
        manifest["policy_digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:"))
    );

    let denied = exec(&workspace, "printf tamper >> \"$NONAUT_CAP_FILE\"");
    assert!(!denied.status.success(), "capability file was writable");
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn timed_out_commands_return_their_partial_output() {
    let root = TestRoot::new("partial-timeout");
    let workspace = root.workspace();
    write_config(&workspace, r#"{"tools":{"timeout_seconds":1}}"#);

    let output = exec(&workspace, "printf before-timeout; sleep 10");
    assert!(!output.status.success(), "command unexpectedly completed");
    let failure = combined(&output);
    assert!(failure.contains("timed out after 1 seconds"), "{failure}");
    assert!(failure.contains("before-timeout"), "{failure}");
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

#[cfg(target_os = "macos")]
#[test]
#[ignore = "provider-free black-box integration test"]
fn headless_exec_can_read_the_macos_tls_certificate_bundle() {
    let root = TestRoot::new("exec-macos-certificates");
    let workspace = root.workspace();
    let output = exec(&workspace, "test -r /etc/ssl/cert.pem");
    assert!(output.status.success(), "{}", combined(&output));
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn explicit_workspace_selection_uses_its_network_policy() {
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
        "workspace": {
            "definitions": {
                "open": {
                    "path": open,
                    "network": {"mode": "open"}
                },
                "blocked": {
                    "path": blocked,
                    "network": {"mode": "blocked"}
                }
            }
        }
    });
    write_config(&session, &serde_json::to_string(&config).unwrap());
    assert!(curl_reaches_listener_in_selected(&session, &open, "open"));
    assert!(!curl_reaches_listener_in_selected(
        &session, &blocked, "blocked"
    ));
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn named_workspace_references_are_read_only_and_other_workspaces_stay_hidden() {
    let root = TestRoot::new("workspace-references");
    let session = root.workspace();
    let primary = session.join("primary");
    let reference = session.join("reference");
    let unrelated = session.join("unrelated");
    fs::create_dir_all(&primary).unwrap();
    fs::create_dir_all(&reference).unwrap();
    fs::create_dir_all(&unrelated).unwrap();
    fs::write(reference.join("api.txt"), "reference-visible").unwrap();
    fs::write(unrelated.join("secret.txt"), "must-stay-hidden").unwrap();
    let config = serde_json::json!({
        "workspace": {
            "definitions": {
                "primary": {
                    "path": primary,
                    "references": {"api": {"path": reference}}
                },
                "unrelated": {"path": unrelated}
            }
        }
    });
    write_config(&session, &serde_json::to_string(&config).unwrap());

    let read = exec_in_selected(
        &session,
        &primary,
        &format!("cat {}/api.txt", reference.display()),
        Some("primary"),
    );
    assert!(read.status.success(), "{}", combined(&read));
    assert_eq!(
        String::from_utf8_lossy(&read.stdout).trim(),
        "reference-visible"
    );

    let write = exec_in_selected(
        &session,
        &primary,
        &format!("printf changed > {}/api.txt", reference.display()),
        Some("primary"),
    );
    assert!(
        !write.status.success(),
        "reference write unexpectedly succeeded"
    );
    assert_eq!(
        fs::read_to_string(reference.join("api.txt")).unwrap(),
        "reference-visible"
    );

    let unrelated_read = exec_in_selected(
        &session,
        &primary,
        &format!("cat {}/secret.txt", unrelated.display()),
        Some("primary"),
    );
    assert!(
        !unrelated_read.status.success(),
        "configured sibling was implicitly readable"
    );
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
    curl_reaches_listener_in_selected(workspace, cwd, ".")
}

fn curl_reaches_listener_in_selected(workspace: &Path, cwd: &Path, selected: &str) -> bool {
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
    let output = exec_in_selected(
        workspace,
        cwd,
        &format!("curl --silent --show-error --max-time 2 http://{address}"),
        (selected != ".").then_some(selected),
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

fn wait_for_after(output: &Arc<Mutex<Vec<u8>>>, offset: usize, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let bytes = output.lock().unwrap();
        if bytes
            .get(offset..)
            .is_some_and(|tail| String::from_utf8_lossy(tail).contains(needle))
        {
            return;
        }
        drop(bytes);
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {needle:?} after output offset {offset}");
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.is_file() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
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
    wait_for(&run.output, "Running shell <shell-command> in nono");
    run.writer.write_all(b"\x1b").unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "Command interrupted");
    run.writer.write_all(b"/q\r").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);

    let output = run.output.lock().unwrap();
    assert!(output.windows(8).any(|window| window == b"\x1b[?1000h"));
    assert!(output.windows(8).any(|window| window == b"\x1b[?1000l"));
    assert!(output.windows(8).any(|window| window == b"\x1b[?1049l"));
    assert!(output.windows(4).any(|window| window == b"\x1b[3J"));
    let config = root.path.join("config/nonaut");
    assert!(config.join("config.json").is_file());
    assert_eq!(fs::read_dir(config).unwrap().count(), 1);
}

#[test]
#[ignore = "provider-free PTY integration test"]
fn tui_reuses_a_sandbox_worker_across_commands() {
    let root = TestRoot::new("tui-worker-reuse");
    let workspace = root.workspace();
    let mut run = spawn_pty(&workspace, &["__nonaut_tui"], &root);
    wait_for(&run.output, "Ask anything");

    run.writer
        .write_all(b"/exec printf $PPID > first-parent.txt\r")
        .unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "Completed shell <shell-command>");
    let outside = root.path.join("outside.txt");
    fs::write(&outside, "hidden").unwrap();
    run.writer
        .write_all(format!("/exec cat {}\r", outside.display()).as_bytes())
        .unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "Failed shell <shell-command>");

    let widened = serde_json::json!({
        "sandbox": {"filesystem": {"read": [outside]}}
    });
    write_config(&workspace, &serde_json::to_string(&widened).unwrap());
    let offset = run.output.lock().unwrap().len();
    run.writer
        .write_all(format!("/exec cat {}\r", outside.display()).as_bytes())
        .unwrap();
    run.writer.flush().unwrap();
    wait_for_after(&run.output, offset, "Failed shell <shell-command>");

    run.writer
        .write_all(b"/exec printf $PPID > second-parent.txt\r")
        .unwrap();
    run.writer.flush().unwrap();
    wait_for_file(&workspace.join("second-parent.txt"));
    run.writer.write_all(b"/q\r").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);

    let first = fs::read_to_string(workspace.join("first-parent.txt")).unwrap();
    let second = fs::read_to_string(workspace.join("second-parent.txt")).unwrap();
    assert_eq!(
        first, second,
        "commands ran under different sandbox workers"
    );
}

#[test]
#[ignore = "provider-free PTY integration test"]
fn tui_command_card_streams_progress_and_elapsed_time() {
    let root = TestRoot::new("tui-command-progress");
    let workspace = root.workspace();
    let mut run = spawn_pty(&workspace, &["__nonaut_tui"], &root);
    wait_for(&run.output, "Ask anything");
    run.writer
        .write_all(b"/exec printf first-line; sleep 2; printf second-line\r")
        .unwrap();
    run.writer.flush().unwrap();

    wait_for(&run.output, "Running shell <shell-command>");
    wait_for(&run.output, "first");
    wait_for(&run.output, "1 output lines");
    wait_for(&run.output, "00:00 / 02:00");
    wait_for(&run.output, "Completed shell <shell-command> · 00:02");
    run.writer.write_all(b"/q\r").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);
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
fn first_launch_setup_saves_the_workspace_and_starts_the_tui() {
    let root = TestRoot::new("workspace-setup");
    let workspace = root.workspace();
    let mut run = spawn_pty(&workspace, &[], &root);
    wait_for(&run.output, "Configure");
    run.writer.write_all(b"\x1b[B\r\r\r\r\r\r\r").unwrap();
    run.writer.flush().unwrap();
    answer_cursor_position(&mut run);
    wait_for(&run.output, "Ask anything");
    run.writer.write_all(b"/q\r").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);

    let config = fs::read_to_string(root.path.join("config/nonaut/config.json")).unwrap();
    let config: serde_json::Value = serde_json::from_str(&config).unwrap();
    assert_eq!(
        config["workspace"]["definitions"]["workspace"]["path"],
        "~/workspace"
    );
}

#[test]
#[ignore = "provider-free PTY integration test"]
fn detached_session_can_be_listed_attached_and_killed() {
    let root = TestRoot::new("session-lifecycle");
    let workspace = root.workspace();
    let mut run = spawn_pty(&workspace, &["--workspace", "."], &root);
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
