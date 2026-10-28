fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let internal = std::env::args()
                .nth(1)
                .is_some_and(|argument| argument.starts_with("__nonaut_"));
            if internal {
                eprintln!("Error: {error:#}");
            } else {
                nonaut::print_error(&error);
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let mode = args.first().map(String::as_str);
    if mode == Some("__nonaut_sandbox") {
        return nonaut::run_sandbox();
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if mode == Some("__nonaut_mcp") {
        let workspace = std::env::args_os()
            .nth(2)
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("MCP workspace was not provided"))?;
        return runtime.block_on(nonaut::run_mcp(workspace));
    }
    if mode == Some("__nonaut_tui") {
        return runtime.block_on(nonaut::run_tui());
    }
    if mode == Some("__nonaut_session") {
        let id = args
            .get(1)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("session id was not provided"))?;
        let workspace = args
            .get(2)
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("session workspace was not provided"))?;
        return runtime.block_on(nonaut::run_session_worker(id, workspace));
    }
    if matches!(mode, Some("help" | "--help" | "-h")) {
        nonaut::print_help();
        return Ok(());
    }
    if matches!(mode, Some("version" | "--version" | "-V")) {
        nonaut::print_version();
        return Ok(());
    }
    if mode == Some("enroll") {
        #[cfg(feature = "control-plane")]
        return runtime.block_on(nonaut::run_enroll(args.into_iter().skip(1).collect()));
        #[cfg(not(feature = "control-plane"))]
        anyhow::bail!(
            "this Nonaut build does not include control-plane support; rebuild with `--features control-plane`"
        );
    }
    if mode == Some("audit") {
        #[cfg(feature = "control-plane")]
        return runtime.block_on(nonaut::run_audit(args.into_iter().skip(1).collect()));
        #[cfg(not(feature = "control-plane"))]
        anyhow::bail!(
            "this Nonaut build does not include control-plane support; rebuild with `--features control-plane`"
        );
    }
    if mode == Some("exec") {
        if matches!(
            args.get(1).map(String::as_str),
            Some("help" | "--help" | "-h")
        ) {
            nonaut::print_command_help(
                "exec",
                "nonaut exec [--cwd DIR] [--] <command>",
                "Run a shell command in a fresh nono sandbox.",
            );
            return Ok(());
        }
        let session_root = std::env::current_dir()?;
        let mut cwd = session_root.clone();
        let mut command_args = Vec::new();
        let mut arguments = args.iter().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--cwd" => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--cwd needs a directory"))?;
                    let path = std::path::PathBuf::from(value);
                    cwd = if path.is_absolute() {
                        path
                    } else {
                        session_root.join(path)
                    };
                }
                "--" => {
                    command_args.extend(arguments.by_ref().cloned());
                    break;
                }
                _ => {
                    command_args.push(argument.clone());
                    command_args.extend(arguments.by_ref().cloned());
                    break;
                }
            }
        }
        let command = command_args.join(" ");
        if command.trim().is_empty() {
            anyhow::bail!("missing command\n\nUsage: nonaut exec [--cwd DIR] [--] <command>");
        }
        let output = runtime.block_on(nonaut::run_exec(cwd, command))?;
        print!("{output}");
        if !output.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    runtime.block_on(nonaut::run(args))
}
