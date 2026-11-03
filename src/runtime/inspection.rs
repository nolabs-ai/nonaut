use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::json;
use std::path::{Path, PathBuf};

use super::integrations::CATALOG;

#[derive(Serialize)]
struct Check {
    pack: String,
    program: String,
    status: &'static str,
    detail: String,
}

pub(crate) async fn run(args: Vec<String>, requested: Option<&str>) -> Result<()> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "help" | "--help" | "-h"))
    {
        crate::cli::print_command_help(
            "runtime",
            "nonaut runtime [show|check|list] [--json] [--config FILE]",
            "Inspect resolved runtime grants or check selected tools inside the sandbox.",
        );
        return Ok(());
    }
    let mut action = None;
    let mut as_json = false;
    for argument in &args {
        match argument.as_str() {
            "--json" if !as_json => as_json = true,
            "show" | "check" | "list" if action.is_none() => action = Some(argument.as_str()),
            _ => bail!("usage: nonaut runtime [show|check|list] [--json] [--config FILE]"),
        }
    }
    if action == Some("list") {
        if as_json {
            println!("{}", serde_json::to_string_pretty(CATALOG)?);
        } else {
            for pack in CATALOG {
                println!("{:<12} {}", pack.name, pack.description);
            }
        }
        return Ok(());
    }
    let workspace = crate::workspace_root(requested)?;
    let policy = crate::policy::pin(&workspace)?;
    if action != Some("check") {
        if as_json {
            println!("{}", policy.capability_manifest_json()?);
        } else {
            println!("{}", crate::policy::describe_full(&workspace)?);
        }
        return Ok(());
    }
    crate::tools::prewarm(&workspace).await?;
    let mut checks = Vec::new();
    for selection in &policy.runtime.packs {
        let Some(pack) = CATALOG.iter().find(|pack| pack.name == selection.name) else {
            continue;
        };
        let Some(probe) = &pack.probe else {
            continue;
        };
        let program = probe.program;
        let path = find_program(&policy.path_entries, program);
        let Some(path) = path else {
            checks.push(Check {
                pack: selection.name.clone(),
                program: program.into(),
                status: "unavailable",
                detail: "not installed on the resolved PATH".into(),
            });
            continue;
        };
        let result = crate::tools::execute(
            &workspace,
            "run_command",
            &json!({"program": path, "args": probe.args, "cwd": workspace, "timeout_seconds": 15}),
        )
        .await;
        let (status, detail) = match result {
            Ok(output) => (
                "passed",
                output
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("completed")
                    .to_owned(),
            ),
            Err(error) => ("failed", format!("{error:#}")),
        };
        checks.push(Check {
            pack: selection.name.clone(),
            program: program.into(),
            status,
            detail,
        });
    }
    let failed = checks.iter().any(|check| check.status != "passed");
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"mode": policy.runtime.mode, "checks": checks, "diagnostics": policy.runtime.diagnostics})
            )?
        );
    } else if checks.is_empty() {
        println!(
            "No built-in runtime probes are selected ({} mode).",
            policy.runtime.mode
        );
    } else {
        for check in checks {
            println!("{:<12} {:<11} {}", check.pack, check.status, check.detail);
        }
    }
    if failed {
        bail!(
            "runtime compatibility checks failed; inspect `nonaut runtime show` and adjust the user policy"
        );
    }
    Ok(())
}

fn find_program(path: &[PathBuf], name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    path.iter()
        .map(|directory| directory.join(name))
        .find(|path| {
            Path::is_file(path)
                && std::fs::metadata(path)
                    .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
        })
}
