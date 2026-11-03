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
    isolate_toolchain_environment(&mut process);
    if let Some(name) = selected_workspace {
        process
            .arg("--config")
            .arg(config_dir(workspace).join(format!("{name}.json")));
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
    let mixed = session.clone();
    let other = root.path.join("other");
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
        "root": mixed, "runtime": {"mode":"auto", "packs":["rust", "python", "node", "go", "bun"]},
        "filesystem": {"readwrite": ["."]}
    });
    write_named_policy(&session, "mixed", &config);

    let allowed = exec_in_selected(
        &session,
        &mixed,
        "cargo --version && printf ':' && python --version && printf ':' && node --version && printf ':' && go version && printf ':' && bun --version",
        Some("mixed"),
    );
    assert!(allowed.status.success(), "{}", combined(&allowed));
    assert!(
        combined(&allowed).contains("rust-profile-ok"),
        "{}",
        combined(&allowed)
    );
    assert!(combined(&allowed).contains("python-profile-ok"));
    assert!(combined(&allowed).contains("node-profile-ok"));
    assert!(combined(&allowed).contains("go-profile-ok"));
    assert!(combined(&allowed).contains("bun-profile-ok"));

    let denied = exec_in_selected(&session, &other, "printf should-not-run", Some("mixed"));
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

fn runtime_command(workspace: &Path, name: &str, action: &str) -> Command {
    let mut command = Command::new(binary());
    isolate_toolchain_environment(&mut command);
    command
        .args(["runtime", action, "--json", "--config"])
        .arg(config_dir(workspace).join(format!("{name}.json")))
        .current_dir(workspace)
        .env("HOME", workspace.parent().unwrap())
        .env(
            "XDG_CONFIG_HOME",
            workspace.parent().unwrap().join("config"),
        );
    command
}

fn isolate_toolchain_environment(command: &mut Command) {
    for name in [
        "CARGO_HOME",
        "RUSTUP_HOME",
        "PYENV_ROOT",
        "NVM_DIR",
        "FNM_DIR",
        "BUN_INSTALL",
        "GOROOT",
    ] {
        command.env_remove(name);
    }
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn runtime_custom_mode_disables_every_builtin() {
    let root = TestRoot::new("runtime-custom");
    let workspace = root.workspace();
    fs::write(workspace.join("Cargo.toml"), "[package]").unwrap();
    write_named_policy(
        &workspace,
        "custom",
        &serde_json::json!({"filesystem":{"readwrite":["."]}, "runtime":{"mode":"custom","environment":{"COMPANY_OPTION":"private-value"}}}),
    );
    let output = runtime_command(&workspace, "custom", "show")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", combined(&output));
    let manifest: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &manifest["runtime_support"]["plan"];
    assert_eq!(plan["mode"], "custom");
    assert_eq!(plan["packs"], serde_json::json!([]));
    assert_eq!(plan["path_entries"], serde_json::json!([]));
    assert!(
        plan["grants"]
            .as_array()
            .unwrap()
            .iter()
            .all(|grant| grant["source"] == "executor")
    );
    assert_eq!(
        plan["environment_sources"],
        serde_json::json!({"COMPANY_OPTION":"runtime.environment"})
    );
    assert!(!combined(&output).contains("private-value"));
    let denied = exec_in_selected(
        &workspace,
        &workspace,
        "/bin/sh -c 'printf must-not-run'",
        Some("custom"),
    );
    assert!(!denied.status.success());
    assert!(
        combined(&denied).contains("could not execute"),
        "{}",
        combined(&denied)
    );
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn runtime_manual_system_pack_runs_commands_and_checks_without_detecting_project_packs() {
    let root = TestRoot::new("runtime-manual");
    let workspace = root.workspace();
    fs::write(workspace.join("Cargo.toml"), "[package]").unwrap();
    write_named_policy(
        &workspace,
        "manual",
        &serde_json::json!({"filesystem":{"readwrite":["."]},"runtime":{"mode":"manual","packs":["system"],"environment":{"COMPANY_OPTION":"configured"}}}),
    );
    let output = exec_in_selected(
        &workspace,
        &workspace,
        "test -z \"$CARGO_HOME\" && printf '%s' \"$COMPANY_OPTION\"",
        Some("manual"),
    );
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "configured\n");
    let check = runtime_command(&workspace, "manual", "check")
        .output()
        .unwrap();
    assert!(check.status.success(), "{}", combined(&check));
    let document: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(document["checks"].as_array().unwrap().len(), 1);
    assert_eq!(document["checks"][0]["status"], "passed");
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn runtime_auto_detects_custom_toolchain_locations_and_isolates_their_state() {
    use std::os::unix::fs::PermissionsExt;
    let root = TestRoot::new("runtime-auto");
    let workspace = root.workspace();
    let installation = root.path.join("company-cargo");
    fs::create_dir_all(installation.join("bin")).unwrap();
    let cargo = installation.join("bin/cargo");
    fs::write(
        &cargo,
        "#!/bin/sh\nprintf 'custom-cargo:%s' \"$CARGO_HOME\"\n",
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    write_named_policy(
        &workspace,
        "auto",
        &serde_json::json!({"filesystem":{"readwrite":["."]},"runtime":{"mode":"auto"}}),
    );
    let output = runtime_command(&workspace, "auto", "show")
        .env("CARGO_HOME", &installation)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", combined(&output));
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        document["runtime_support"]["plan"]["packs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|pack| pack["name"] == "rust" && pack["reason"] == "host installation")
    );
    let output = Command::new(binary())
        .args(["exec", "--config"])
        .arg(config_dir(&workspace).join("auto.json"))
        .args(["--", "cargo --version"])
        .current_dir(&workspace)
        .env("HOME", &root.path)
        .env("XDG_CONFIG_HOME", root.path.join("config"))
        .env("CARGO_HOME", &installation)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", combined(&output));
    assert!(combined(&output).contains("custom-cargo:"));
    assert!(!combined(&output).contains(&format!("custom-cargo:{}", installation.display())));
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn runtime_imports_replace_selection_environment_and_grants_while_preserving_denials() {
    let root = TestRoot::new("runtime-import");
    let workspace = root.workspace();
    write_named_policy(
        &workspace,
        "base",
        &serde_json::json!({"filesystem":{"readwrite":["."]},"runtime":{"mode":"manual","packs":["system","rust"],"filesystem":{"read":[root.path],"deny":[root.path.join("secret")]},"environment":{"INHERITED":"must-disappear"}}}),
    );
    write_named_policy(
        &workspace,
        "child",
        &serde_json::json!({"extends":["base.json"],"runtime":{"mode":"custom","environment":{"FINAL":"configured"}}}),
    );
    let output = runtime_command(&workspace, "child", "show")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", combined(&output));
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &document["runtime_support"]["plan"];
    assert_eq!(plan["packs"], serde_json::json!([]));
    assert_eq!(
        plan["environment_sources"],
        serde_json::json!({"FINAL":"runtime.environment"})
    );
    assert_eq!(
        document["deny"][0],
        root.path.join("secret").to_str().unwrap()
    );
    assert!(
        plan["grants"]
            .as_array()
            .unwrap()
            .iter()
            .all(|grant| grant["kind"] == "executor")
    );
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn runtime_custom_explicit_grants_can_replace_system_support_without_hidden_packs() {
    let root = TestRoot::new("runtime-explicit");
    let workspace = root.workspace();
    write_named_policy(
        &workspace,
        "system",
        &serde_json::json!({"filesystem":{"readwrite":["."]},"runtime":{"mode":"manual","packs":["system"]}}),
    );
    let output = runtime_command(&workspace, "system", "show")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", combined(&output));
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let plan = &document["runtime_support"]["plan"];
    let grants = plan["grants"].as_array().unwrap();
    let paths = |writable| {
        grants
            .iter()
            .filter(|grant| grant["source"] == "system" && (grant["access"] != "read") == writable)
            .map(|grant| grant["path"].clone())
            .collect::<Vec<_>>()
    };
    let bins = plan["path_entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| PathBuf::from(entry.as_str().unwrap()))
        .collect::<Vec<_>>();
    write_named_policy(
        &workspace,
        "custom",
        &serde_json::json!({"filesystem":{"readwrite":["."]},"runtime":{"mode":"custom","filesystem":{"read":paths(false),"readwrite":paths(true)},"environment":{"PATH":std::env::join_paths(bins).unwrap().to_str().unwrap()}}}),
    );
    fs::write(root.path.join("outside-secret"), "must-stay-private").unwrap();
    let output = exec_in_selected(
        &workspace,
        &workspace,
        "printf explicit-runtime-ok",
        Some("custom"),
    );
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "explicit-runtime-ok\n"
    );
    let denied = exec_in_selected(
        &workspace,
        &workspace,
        "cat ../outside-secret",
        Some("custom"),
    );
    assert!(!denied.status.success());
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn runtime_detects_custom_homebrew_prefixes_but_ignores_project_controlled_prefixes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = TestRoot::new("runtime-homebrew");
    let workspace = root.workspace();
    let prefix = root.path.join("company-brew");
    for directory in ["Cellar/company/bin", "opt", "bin"] {
        fs::create_dir_all(prefix.join(directory)).unwrap();
    }
    let tool = prefix.join("Cellar/company/bin/company-tool");
    fs::write(&tool, "#!/bin/sh\nprintf custom-brew-ok\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&tool, prefix.join("bin/company-tool")).unwrap();
    write_named_policy(
        &workspace,
        "auto",
        &serde_json::json!({"filesystem":{"readwrite":["."]},"runtime":{"mode":"auto"}}),
    );
    let output = Command::new(binary())
        .args(["exec", "--config"])
        .arg(config_dir(&workspace).join("auto.json"))
        .args(["--", "company-tool"])
        .current_dir(&workspace)
        .env("HOME", &root.path)
        .env("XDG_CONFIG_HOME", root.path.join("config"))
        .env("HOMEBREW_PREFIX", &prefix)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "custom-brew-ok\n");
    let untrusted = workspace.join("malicious-brew");
    fs::create_dir_all(untrusted.join("Cellar")).unwrap();
    fs::create_dir_all(untrusted.join("opt")).unwrap();
    let output = runtime_command(&workspace, "auto", "show")
        .env("HOMEBREW_PREFIX", &untrusted)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", combined(&output));
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !document["runtime_support"]["plan"]["grants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|grant| Path::new(grant["path"].as_str().unwrap()).starts_with(&untrusted))
    );
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn replacing_command_capture_with_a_secret_symlink_never_exposes_host_data() {
    let root = TestRoot::new("capture-symlink");
    let workspace = root.workspace();
    let secret = root.path.join("host-secret");
    fs::write(&secret, "private-host-data-must-not-appear").unwrap();
    write_config(&workspace, r#"{"tools":{"timeout_seconds":1}}"#);
    let output = exec(
        &workspace,
        &format!(
            "printf captured-before-timeout; for f in \"$HOME\"/.nonaut-output-*; do rm -f \"$f\"; ln -s '{}' \"$f\"; done; sleep 3",
            secret.display()
        ),
    );
    assert!(!output.status.success());
    let text = combined(&output);
    assert!(text.contains("timed out"), "{text}");
    assert!(text.contains("captured-before-timeout"), "{text}");
    assert!(
        !text.contains("private-host-data-must-not-appear"),
        "{text}"
    );
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn broad_runtime_writes_cannot_make_the_capability_manifest_writable() {
    let root = TestRoot::new("manifest-write-grant");
    let workspace = root.workspace();
    let cache = root.path.join(".cache");
    fs::create_dir_all(&cache).unwrap();
    write_named_policy(
        &workspace,
        "broad",
        &serde_json::json!({"filesystem":{"readwrite":["."]},"runtime":{"mode":"custom","filesystem":{"readwrite":[cache]}}}),
    );
    let output = runtime_command(&workspace, "broad", "show")
        .env("XDG_CACHE_HOME", &cache)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        combined(&output).contains("read-only capability manifest directory"),
        "{}",
        combined(&output)
    );
}

#[test]
#[ignore = "provider-free native runtime integration test"]
fn runtime_auto_builds_native_code_and_runs_python_without_startup_warnings() {
    let root = TestRoot::new("native-runtime");
    let workspace = root.workspace();
    fs::write(
        workspace.join("native.c"),
        "#include <stdio.h>\nint main(void) { puts(\"native-ok\"); return 0; }\n",
    )
    .unwrap();
    write_config(&workspace, r#"{"runtime":{"mode":"auto"}}"#);
    let output = exec(
        &workspace,
        "cc native.c -o native && ./native && python3 -c 'print(\"python-ok\")'",
    );
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "native-ok\npython-ok\n"
    );
    assert!(output.stderr.is_empty(), "{}", combined(&output));
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
    assert!(
        output.stdout.is_empty(),
        "silent command returned fabricated output: {}",
        combined(&output)
    );
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

#[cfg(target_os = "macos")]
#[test]
#[ignore = "provider-free Apple toolchain integration test"]
fn macos_sdk_lookup_uses_a_private_cache_without_sandbox_warnings() {
    if !Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .output()
        .unwrap()
        .status
        .success()
    {
        return;
    }
    let root = TestRoot::new("sdk-cache");
    let workspace = root.workspace();
    let output = exec(
        &workspace,
        r#"case "$xcrun_db" in "$HOME"/tmp/*) ;; *) exit 1 ;; esac; xcrun --show-sdk-path && cc --version"#,
    );
    let text = combined(&output);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("MacOSX"), "{text}");
    assert!(!text.contains("couldn't create cache file"), "{text}");
    assert!(!text.contains("Operation not permitted"), "{text}");
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "provider-free Homebrew runtime integration test"]
fn macos_homebrew_tools_can_load_their_installed_libraries() {
    let Some(git) = ["/opt/homebrew/bin/git", "/usr/local/bin/git"]
        .into_iter()
        .find(|path| Path::new(path).exists())
    else {
        return;
    };
    let root = TestRoot::new("homebrew-runtime");
    let workspace = root.workspace();
    let output = exec(&workspace, &format!("{git} --version"));
    assert!(output.status.success(), "{}", combined(&output));
    assert!(combined(&output).contains("git version"));
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
    write_config(&workspace, r#"{"network":{"mode":"blocked"}}"#);
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
    write_config(&workspace, r#"{"network":{"mode":"open"}}"#);
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
fn named_references_are_self_contained_and_read_only() {
    let root = TestRoot::new("project-references");
    let workspace = root.workspace();
    let reference = root.path.join("reference");
    fs::create_dir_all(&reference).unwrap();
    fs::write(reference.join("api.txt"), "reference-visible").unwrap();
    let unrelated = root.path.join("secret.txt");
    fs::write(&unrelated, "hidden").unwrap();
    write_config(
        &workspace,
        &serde_json::json!({
            "references": {"api": {"path": "../reference", "access": "read"}}
        })
        .to_string(),
    );
    let read = exec(&workspace, &format!("cat {}/api.txt", reference.display()));
    assert!(read.status.success(), "{}", combined(&read));
    assert_eq!(
        String::from_utf8_lossy(&read.stdout).trim(),
        "reference-visible"
    );
    let write = exec(
        &workspace,
        &format!("printf changed > {}/api.txt", reference.display()),
    );
    assert!(!write.status.success());
    assert_eq!(
        fs::read_to_string(reference.join("api.txt")).unwrap(),
        "reference-visible"
    );
    assert!(
        !exec(&workspace, &format!("cat {}", unrelated.display()))
            .status
            .success()
    );
}

fn project_policy_path(workspace: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let root = workspace.canonicalize().unwrap();
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    let hash: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    config_dir(workspace).join("projects").join(format!(
        "{}-{hash}.json",
        root.file_name().unwrap().to_string_lossy()
    ))
}

fn write_named_policy(workspace: &Path, name: &str, config: &serde_json::Value) {
    fs::create_dir_all(config_dir(workspace)).unwrap();
    fs::write(
        config_dir(workspace).join(format!("{name}.json")),
        config.to_string(),
    )
    .unwrap();
}

fn write_config(workspace: &Path, config_text: &str) {
    let mut config: serde_json::Value = serde_json::from_str(config_text).unwrap();
    if config.get("filesystem").is_none() {
        config["filesystem"] = serde_json::json!({"readwrite": ["."]});
    }
    let path = project_policy_path(workspace);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, config.to_string()).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "provider-free Homebrew runtime integration test"]
fn macos_homebrew_node_can_load_openssl_configuration_without_ambient_home_access() {
    let node = ["/opt/homebrew/bin/node", "/usr/local/bin/node"]
        .into_iter()
        .find(|path| Path::new(path).is_file());
    let Some(node) = node else {
        return;
    };
    let root = TestRoot::new("node-openssl");
    let workspace = root.workspace();
    write_config(&workspace, r#"{"runtime":{"mode":"auto"}}"#);
    let output = exec(
        &workspace,
        &format!("{node} -e 'console.log(\"node-openssl-ok\")'"),
    );
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "node-openssl-ok\n");
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
    let bytes = output.lock().unwrap();
    let start = offset
        .max(bytes.len().saturating_sub(12_000))
        .min(bytes.len());
    panic!(
        "timed out waiting for {needle:?} after output offset {offset}\n{}",
        String::from_utf8_lossy(&bytes[start..])
    );
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
    wait_for(&run.output, "Running shell <shell-command>");
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
    wait_for(&run.output, "✓ shell <shell-command>");
    let outside = root.path.join("outside.txt");
    fs::write(&outside, "hidden").unwrap();
    run.writer
        .write_all(format!("/exec cat {}\r", outside.display()).as_bytes())
        .unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "✗ shell <shell-command>");

    let widened = serde_json::json!({
        "references": {"outside": {"path": outside, "access": "read"}}
    });
    write_config(&workspace, &serde_json::to_string(&widened).unwrap());
    fs::create_dir(workspace.join("nested")).unwrap();
    let offset = run.output.lock().unwrap().len();
    run.writer
        .write_all(format!("/exec --cwd nested cat {}\r", outside.display()).as_bytes())
        .unwrap();
    run.writer.flush().unwrap();
    // A distinct directory gives this execution a unique heading, so a repaint
    // of the first failure cannot be mistaken for the second completion.
    wait_for_after(&run.output, offset, "✗ shell <shell-command> · nested");

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
        .write_all(b"/exec printf first-%s line; sleep 2; printf second-%s line\r")
        .unwrap();
    run.writer.flush().unwrap();

    wait_for(&run.output, "Running shell <shell-command>");
    wait_for(&run.output, "  first-line");
    wait_for(&run.output, "✓ shell <shell-command>");
    run.writer.write_all(b"/details\r").unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "Tool: exec");
    wait_for(&run.output, "first-linesecond-line");
    wait_for(&run.output, "q/Esc close");
    run.writer
        .write_all(b"q/exec printf modal-returned > modal-returned.txt\r")
        .unwrap();
    run.writer.flush().unwrap();
    wait_for_file(&workspace.join("modal-returned.txt"));
    run.writer.write_all(b"/q\r").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);
}

#[test]
#[ignore = "provider-free PTY integration test"]
fn tui_details_scroll_and_close_back_to_the_command_pane() {
    let root = TestRoot::new("tui-details");
    let workspace = root.workspace();
    let mut run = spawn_pty(&workspace, &["__nonaut_tui"], &root);
    wait_for(&run.output, "Ask anything");
    run.writer.write_all(b"/exec i=1; while [ $i -le 80 ]; do printf 'details-row-%s\\n' $i; i=$((i+1)); done; printf modal-bottom-marker\r").unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "✓ shell <shell-command>");
    run.writer.write_all(b"/details\r").unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "Tool details");
    let offset = run.output.lock().unwrap().len();
    run.writer.write_all(b"\x1b[F\x0c").unwrap();
    run.writer.flush().unwrap();
    answer_cursor_position(&mut run);
    wait_for_after(&run.output, offset, "modal-bottom-marker");
    let offset = run.output.lock().unwrap().len();
    run.writer.write_all(b"\x1b").unwrap();
    run.writer.flush().unwrap();
    wait_for_after(&run.output, offset, "Ask anything");
    run.writer
        .write_all(b"/exec printf closed > details-closed.txt\r")
        .unwrap();
    run.writer.flush().unwrap();
    wait_for_file(&workspace.join("details-closed.txt"));
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
    wait_for(&run.output, "Review");
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
    fs::write(workspace.join("Cargo.toml"), "[package]\nname='demo'").unwrap();
    let mut run = spawn_pty(&workspace, &[], &root);
    wait_for(&run.output, "Create policy and launch?");
    run.writer.write_all(b"y\n").unwrap();
    run.writer.flush().unwrap();
    answer_cursor_position(&mut run);
    wait_for(&run.output, "Ask anything");
    run.writer.write_all(b"/permissions\r").unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "Workspace  workspace");
    run.writer.write_all(b"/q\r").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);

    let config = fs::read_to_string(project_policy_path(&workspace)).unwrap();
    let config: serde_json::Value = serde_json::from_str(&config).unwrap();
    assert_eq!(
        config["root"],
        serde_json::json!(workspace.canonicalize().unwrap())
    );
    assert_eq!(config["filesystem"], serde_json::json!({"readwrite":["."]}));
    assert!(config.get("references").is_none());
    assert_eq!(config["runtime"]["mode"], "auto");

    let shown = Command::new(binary())
        .arg("config")
        .current_dir(&workspace)
        .env("HOME", &root.path)
        .env("XDG_CONFIG_HOME", root.path.join("config"))
        .output()
        .unwrap();
    assert!(shown.status.success(), "{}", combined(&shown));
    let shown = String::from_utf8_lossy(&shown.stdout);
    assert!(shown.contains("\"workspace\""), "{shown}");
    assert!(shown.contains("\"auto\""), "{shown}");
}

#[test]
#[ignore = "provider-free PTY integration test"]
fn detached_session_can_be_listed_attached_and_killed() {
    let root = TestRoot::new("session-lifecycle");
    let workspace = root.workspace();
    write_config(&workspace, "{}");
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

#[test]
#[ignore = "provider-free black-box integration test"]
fn supplied_rootless_policy_is_selected_without_merging_saved_grants() {
    let root = TestRoot::new("supplied-policy");
    let workspace = root.workspace();
    fs::write(workspace.join("input.txt"), "visible").unwrap();
    write_config(&workspace, "{}");
    write_named_policy(
        &workspace,
        "readonly",
        &serde_json::json!({"filesystem":{"read":["."]}}),
    );
    let read = exec_in_selected(&workspace, &workspace, "cat input.txt", Some("readonly"));
    assert!(read.status.success(), "{}", combined(&read));
    let write = exec_in_selected(
        &workspace,
        &workspace,
        "printf x > output.txt",
        Some("readonly"),
    );
    assert!(
        !write.status.success(),
        "saved write permissions leaked into explicit config"
    );
    write_named_policy(
        &workspace,
        "wrong",
        &serde_json::json!({"root": workspace.parent().unwrap()}),
    );
    let mismatch = exec_in_selected(&workspace, &workspace, "printf unexpected", Some("wrong"));
    assert!(!mismatch.status.success());
    assert!(combined(&mismatch).contains("does not match active project"));
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn policies_and_application_settings_cannot_be_modified_from_commands() {
    let root = TestRoot::new("protected-config");
    let workspace = root.workspace();
    write_config(&workspace, "{}");
    let source = project_policy_path(&workspace);
    let denied = exec(
        &workspace,
        &format!("printf tampered > '{}'", source.display()),
    );
    assert!(!denied.status.success());
    assert!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(source).unwrap()).is_ok()
    );
    let denied = exec(
        &workspace,
        &format!(
            "printf tampered > '{}'",
            config_dir(&workspace).join("config.json").display()
        ),
    );
    assert!(!denied.status.success());
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn deny_within_project_is_enforced_or_rejected_before_launch() {
    let root = TestRoot::new("project-deny");
    let workspace = root.workspace();
    fs::write(workspace.join("credentials.env"), "secret").unwrap();
    fs::write(workspace.join("ordinary.txt"), "ordinary").unwrap();
    write_config(
        &workspace,
        r#"{"filesystem":{"readwrite":["."],"deny":["./credentials.env"]}}"#,
    );
    let denied = exec(&workspace, "cat credentials.env");
    assert!(!denied.status.success());
    #[cfg(target_os = "linux")]
    assert!(combined(&denied).contains("Linux Landlock cannot enforce deny"));
    #[cfg(target_os = "macos")]
    {
        let ordinary = exec(&workspace, "cat ordinary.txt");
        assert!(ordinary.status.success(), "{}", combined(&ordinary));
        assert!(
            !exec(&workspace, "printf leaked > credentials.env")
                .status
                .success()
        );
        assert_eq!(
            fs::read_to_string(workspace.join("credentials.env")).unwrap(),
            "secret"
        );
        std::os::unix::fs::symlink(workspace.join("credentials.env"), workspace.join("alias"))
            .unwrap();
        assert!(!exec(&workspace, "cat alias").status.success());
        assert!(
            !exec(&workspace, "ln credentials.env hardlink && cat hardlink")
                .status
                .success()
        );
        assert!(
            !exec(&workspace, "mv credentials.env renamed && cat renamed")
                .status
                .success()
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "provider-free black-box integration test"]
fn platform_runtime_does_not_expose_unrelated_private_tmp_files() {
    let root = TestRoot::new("runtime-private");
    let workspace = root.workspace();
    let secret = root.runtime.join("secret");
    fs::write(&secret, "hidden").unwrap();
    let denied = exec(&workspace, &format!("cat '{}'", secret.display()));
    assert!(
        !denied.status.success(),
        "broad runtime grant exposed private temporary files"
    );
    let denied = exec(
        &workspace,
        &format!("printf tampered > '{}'", secret.display()),
    );
    assert!(
        !denied.status.success(),
        "shared temporary directories were writable"
    );
    assert_eq!(fs::read_to_string(secret).unwrap(), "hidden");
}

#[test]
#[ignore = "provider-free real toolchain integration test"]
fn rust_pack_runs_real_offline_cargo_tests() {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    if !home.join(".cargo/bin/cargo").exists() {
        return;
    }
    let root = TestRoot::new("real-cargo");
    let workspace = root.workspace();
    fs::create_dir(workspace.join("src")).unwrap();
    // This project is nested inside Nonaut's checkout. An explicit workspace
    // boundary keeps Cargo from probing parent manifests outside its grants.
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = 'sandbox-probe'\nversion = '0.1.0'\nedition = '2024'\n\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "#[test] fn inside_sandbox() { assert_eq!(2 + 2, 4); }\n",
    )
    .unwrap();
    write_named_policy(
        &workspace,
        "rust",
        &serde_json::json!({
            "runtime": {"mode":"auto", "packs":["rust"]}, "filesystem": {"readwrite":["."]}
        }),
    );
    let output = Command::new(binary())
        .args(["--config"])
        .arg(config_dir(&workspace).join("rust.json"))
        .args(["exec", "--", "cargo test --offline"])
        .current_dir(&workspace)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", root.path.join("config"))
        .env("XDG_CACHE_HOME", root.path.join("cache"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", combined(&output));
    assert!(combined(&output).contains("1 passed"));
    assert!(workspace.join("target").is_dir());
}

#[test]
#[ignore = "provider-free PTY integration test"]
fn cancelling_first_setup_writes_no_project_policy() {
    let root = TestRoot::new("cancel-setup");
    let workspace = root.workspace();
    let mut run = spawn_pty(&workspace, &[], &root);
    wait_for(&run.output, "Create policy and launch?");
    run.writer.write_all(b"n\n").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);
    assert!(!project_policy_path(&workspace).exists());
    assert!(!workspace.join("nonaut.json").exists());
}

#[test]
#[ignore = "provider-free black-box integration test"]
fn imported_policy_resolves_permission_paths_from_project_root() {
    let root = TestRoot::new("imported-policy");
    let workspace = root.workspace();
    fs::write(workspace.join("input.txt"), "project-input").unwrap();
    let directory = config_dir(&workspace).join("shared");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("base.json"),
        r#"{"filesystem":{"read":["."]}}"#,
    )
    .unwrap();
    write_named_policy(
        &workspace,
        "imported",
        &serde_json::json!({"extends":["shared/base.json"]}),
    );
    let output = exec_in_selected(&workspace, &workspace, "cat input.txt", Some("imported"));
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "project-input"
    );
    assert!(
        !exec_in_selected(
            &workspace,
            &workspace,
            "printf unexpected > output.txt",
            Some("imported")
        )
        .status
        .success()
    );
}

#[test]
#[ignore = "provider-free PTY integration test"]
fn tui_pastes_multiline_commands_without_submitting_them() {
    let root = TestRoot::new("literal-paste");
    let workspace = root.workspace();
    let mut run = spawn_pty(&workspace, &["__nonaut_tui"], &root);
    wait_for(&run.output, "Ask anything");
    run.writer
        .write_all(b"\x1b[200~/exec printf unexpected > pasted.txt\nsecond-line\x1b[201~")
        .unwrap();
    run.writer.flush().unwrap();
    wait_for(&run.output, "second-line");
    assert!(!workspace.join("pasted.txt").exists());
    assert!(!String::from_utf8_lossy(&run.output.lock().unwrap()).contains("Running shell"));
    run.writer.write_all(b"\x03").unwrap();
    run.writer.flush().unwrap();
    wait_for_exit(&mut run.child);
}
