#[cfg(feature = "control-plane")]
mod enabled {
    use std::path::Path;

    use anyhow::{Result, bail};
    use nonaut_control_plane::{Client, Enrollment, SessionContext};

    pub(crate) type AuditSession = nonaut_control_plane::AuditSession;

    pub(crate) async fn run_enroll(args: Vec<String>) -> Result<()> {
        let client = Client::from_environment()?;
        if matches!(args.first().map(String::as_str), Some("status")) {
            if let Some(status) = client.status()? {
                println!("Enrolled for audit delivery.");
                println!("  Platform: {}", status.platform_url);
                println!("  Tenant:   {}", status.tenant_id);
                println!("  Subject:  {}", status.subject_id);
            } else {
                println!("Not enrolled with a control plane.");
            }
            return Ok(());
        }

        let mut url = None;
        let mut token = None;
        let mut name = None;
        let mut arguments = args.iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--url" => url = arguments.next().map(String::as_str),
                "--token" => token = arguments.next().map(String::as_str),
                "--name" => name = arguments.next().map(String::as_str),
                "help" | "--help" | "-h" => {
                    crate::print_command_help(
                        "enroll",
                        "nonaut enroll --url URL --token TOKEN [--name NAME]\n  nonaut enroll status",
                        "Enroll this Nonaut device for audit delivery.",
                    );
                    return Ok(());
                }
                unknown => bail!("unknown enroll option `{unknown}`"),
            }
        }
        let url = url.ok_or_else(|| anyhow::anyhow!("--url is required"))?;
        let token = token.ok_or_else(|| anyhow::anyhow!("--token is required"))?;
        let default_name = std::env::var("HOSTNAME").unwrap_or_else(|_| "Nonaut device".to_owned());
        let status = client
            .enroll(Enrollment {
                url,
                token,
                display_name: Some(name.unwrap_or(&default_name)),
            })
            .await?;
        println!("Enrolled for audit delivery.");
        println!("  Tenant:   {}", status.tenant_id);
        println!("  Subject:  {}", status.subject_id);
        println!("  Platform: {}", status.platform_url);
        Ok(())
    }

    pub(crate) fn run_unenroll(args: Vec<String>) -> Result<()> {
        if matches!(
            args.first().map(String::as_str),
            Some("help" | "--help" | "-h")
        ) {
            crate::print_command_help(
                "unenroll",
                "nonaut unenroll",
                "Remove this device's local control-plane enrollment credentials.",
            );
            return Ok(());
        }
        if !args.is_empty() {
            bail!("usage: nonaut unenroll");
        }

        let client = Client::from_environment()?;
        if client.unenroll()? {
            println!("Removed local control-plane enrollment credentials.");
            println!(
                "Queued audit data was retained. Revoke the device in the control plane if needed."
            );
        } else {
            println!("Not enrolled with a control plane.");
        }
        Ok(())
    }

    pub(crate) async fn run_audit(args: Vec<String>) -> Result<()> {
        let client = Client::from_environment()?;
        match args.first().map(String::as_str) {
            Some("status") => {
                println!(
                    "Audit delivery\n  Enrolled: {}\n  Queued:   {}",
                    if client.status()?.is_some() {
                        "yes"
                    } else {
                        "no"
                    },
                    client.queued_count()?
                );
                Ok(())
            }
            Some("sync") => {
                let report = client.sync().await?;
                println!(
                    "Audit sync: {} delivered, {} remaining",
                    report.delivered, report.remaining
                );
                if report.remaining == 0 {
                    Ok(())
                } else {
                    bail!("some audit sessions remain queued; retry `nonaut audit sync`")
                }
            }
            Some("help" | "--help" | "-h") | None => {
                crate::print_command_help(
                    "audit",
                    "nonaut audit status\n  nonaut audit sync",
                    "Inspect or retry durable audit delivery.",
                );
                Ok(())
            }
            Some(unknown) => bail!("unknown audit command `{unknown}`"),
        }
    }

    pub(crate) fn start_session(
        workspace: &Path,
        provider: Option<&str>,
        model: Option<&str>,
        reasoning: Option<&str>,
    ) -> Result<Option<AuditSession>> {
        let client = Client::from_environment()?;
        if client.status()?.is_none() {
            return Ok(None);
        }
        let policy = crate::policy::audit_context(workspace)?;
        let workspace_name = policy
            .workspace
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        AuditSession::start(
            &policy.workspace,
            SessionContext {
                nonaut_version: env!("CARGO_PKG_VERSION").to_owned(),
                provider: provider.map(str::to_owned),
                model: model.map(str::to_owned),
                reasoning: reasoning.map(str::to_owned),
                workspace_name,
                profiles: policy.profiles,
                policy_digest: Some(policy.policy_digest),
                network_mode: Some(policy.network_mode),
                sandbox: "nono".to_owned(),
                os: std::env::consts::OS.to_owned(),
                architecture: std::env::consts::ARCH.to_owned(),
            },
        )
        .map(Some)
    }

    pub(crate) fn finish_session(session: AuditSession, success: bool) -> Result<()> {
        let session = session.finish(success)?;
        let client = Client::from_environment()?;
        client.queue(session)?;
        tokio::spawn(async move {
            let _ = client.sync().await;
        });
        Ok(())
    }

    pub(crate) fn record_usage(session: &mut Option<AuditSession>, tokens: u64) {
        if let Some(session) = session {
            session.record_usage(tokens);
        }
    }

    pub(crate) fn tool_started(
        session: &mut Option<AuditSession>,
        context: &crate::tools::ToolContext,
    ) -> Option<Result<()>> {
        let session = session.as_mut()?;
        Some((|| {
            let context = serde_json::from_value(serde_json::to_value(context)?)?;
            session.tool_started(&context)
        })())
    }

    pub(crate) fn tool_finished(
        session: &mut Option<AuditSession>,
        call_id: &str,
        tool: &str,
        success: bool,
        output: &str,
        failure: Option<&crate::tools::ToolFailure>,
    ) -> Option<Result<()>> {
        let session = session.as_mut()?;
        let decision = crate::tools::audit_decision(success, failure);
        let change = crate::tools::audit_change(tool, success, output);
        Some(session.tool_finished(nonaut_control_plane::ToolOutcome {
            call_id,
            tool,
            success,
            decision,
            reason: failure.map(|failure| failure.reason),
            exit_code: failure.and_then(|failure| failure.exit_code),
            output_bytes: output.len() as u64,
            change,
        }))
    }
}

#[cfg(feature = "control-plane")]
pub(crate) use enabled::*;

#[cfg(not(feature = "control-plane"))]
mod disabled {
    use std::path::Path;

    use anyhow::Result;

    pub(crate) struct AuditSession;

    pub(crate) fn start_session(
        _workspace: &Path,
        _provider: Option<&str>,
        _model: Option<&str>,
        _reasoning: Option<&str>,
    ) -> Result<Option<AuditSession>> {
        Ok(None)
    }

    pub(crate) fn finish_session(_session: AuditSession, _success: bool) -> Result<()> {
        Ok(())
    }

    pub(crate) fn record_usage(_session: &mut Option<AuditSession>, _tokens: u64) {}

    pub(crate) fn tool_started(
        _session: &mut Option<AuditSession>,
        _context: &crate::tools::ToolContext,
    ) -> Option<Result<()>> {
        None
    }

    pub(crate) fn tool_finished(
        _session: &mut Option<AuditSession>,
        _call_id: &str,
        _tool: &str,
        _success: bool,
        _output: &str,
        _failure: Option<&crate::tools::ToolFailure>,
    ) -> Option<Result<()>> {
        None
    }
}

#[cfg(not(feature = "control-plane"))]
pub(crate) use disabled::*;
