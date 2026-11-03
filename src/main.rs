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
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let requested_config = extract_config(&mut args)?;
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
    if mode == Some("unenroll") {
        #[cfg(feature = "control-plane")]
        return nonaut::run_unenroll(args.into_iter().skip(1).collect());
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
    if matches!(mode, Some("config")) {
        if matches!(
            args.get(1).map(String::as_str),
            Some("help" | "--help" | "-h")
        ) {
            nonaut::print_command_help(
                "config",
                "nonaut config",
                "Show the current project and configuration file.",
            );
            return Ok(());
        }
        if args.len() != 1 {
            anyhow::bail!("usage: nonaut config");
        }
        return nonaut::run_project_config(requested_config.as_deref());
    }
    if mode == Some("runtime") {
        return runtime.block_on(nonaut::run_runtime(
            args.into_iter().skip(1).collect(),
            requested_config.as_deref(),
        ));
    }
    if mode == Some("exec") {
        if matches!(
            args.get(1).map(String::as_str),
            Some("help" | "--help" | "-h")
        ) {
            nonaut::print_command_help(
                "exec",
                "nonaut exec [--config FILE] [--cwd DIR] [--] <command>",
                "Run a shell command in a policy-pinned nono sandbox.",
            );
            return Ok(());
        }
        let session_root = nonaut::workspace_root(requested_config.as_deref())?;
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
            anyhow::bail!(
                "missing command\n\nUsage: nonaut exec [--config FILE] [--cwd DIR] [--] <command>"
            );
        }
        let output = runtime.block_on(nonaut::run_exec(session_root, cwd, command))?;
        print!("{output}");
        if !output.is_empty() && !output.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    runtime.block_on(nonaut::run(args, requested_config))
}

fn extract_config(args: &mut Vec<String>) -> anyhow::Result<Option<String>> {
    let mut selected = None;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--" {
            break;
        }
        if matches!(args[index].as_str(), "--config") {
            let value = args
                .get(index + 1)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{} needs a policy file", args[index]))?;
            if selected.replace(value).is_some() {
                anyhow::bail!("--config may be supplied only once");
            }
            args.drain(index..=index + 1);
            continue;
        }
        index += 1;
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_selection_is_global_but_stops_at_command_separator() {
        let mut args = vec![
            "exec".to_owned(),
            "--config".to_owned(),
            "project".to_owned(),
            "--".to_owned(),
            "printf --config untouched".to_owned(),
        ];
        assert_eq!(
            extract_config(&mut args).unwrap().as_deref(),
            Some("project")
        );
        assert_eq!(args, ["exec", "--", "printf --config untouched"]);
    }

    #[test]
    fn config_selection_rejects_missing_and_duplicate_values() {
        assert!(extract_config(&mut vec!["--config".to_owned()]).is_err());
        assert!(
            extract_config(&mut vec![
                "--config".to_owned(),
                "one".to_owned(),
                "--config".to_owned(),
                "two".to_owned(),
            ])
            .is_err()
        );
    }
}
