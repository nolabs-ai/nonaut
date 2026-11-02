use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use crossterm::{Command as _, clipboard::CopyToClipboard};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

const DEADLINE: Duration = Duration::from_secs(2);

pub(super) async fn write(text: &str) -> Result<()> {
    for (program, args) in native_commands(false) {
        if run(program, args, Some(text), DEADLINE).await.is_ok() {
            return Ok(());
        }
    }
    let mut sequence = String::new();
    CopyToClipboard::to_clipboard_from(text).write_ansi(&mut sequence)?;
    let mut stdout = tokio::io::stdout();
    stdout.write_all(sequence.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

pub(super) async fn read() -> Result<String> {
    for (program, args) in native_commands(true) {
        if let Ok(bytes) = run(program, args, None, DEADLINE).await {
            return String::from_utf8(bytes).context("clipboard does not contain UTF-8 text");
        }
    }
    bail!("clipboard unavailable; use your terminal's paste shortcut")
}

fn native_commands(read: bool) -> Vec<(&'static str, &'static [&'static str])> {
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        return Vec::new();
    }
    #[cfg(target_os = "macos")]
    {
        vec![(
            if read {
                "/usr/bin/pbpaste"
            } else {
                "/usr/bin/pbcopy"
            },
            &[],
        )]
    }
    #[cfg(target_os = "linux")]
    {
        let mut commands: Vec<(&str, &[&str])> = Vec::new();
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            commands.push(if read {
                ("/usr/bin/wl-paste", &["--no-newline"])
            } else {
                ("/usr/bin/wl-copy", &[])
            });
        }
        if std::env::var_os("DISPLAY").is_some() {
            commands.push((
                "/usr/bin/xclip",
                if read {
                    &["-selection", "clipboard", "-o"]
                } else {
                    &["-selection", "clipboard", "-i"]
                },
            ));
        }
        commands
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = read;
        Vec::new()
    }
}

async fn run(
    program: &str,
    args: &[&str],
    input: Option<&str>,
    deadline: Duration,
) -> Result<Vec<u8>> {
    timeout(deadline, async {
        let mut child = Command::new(program)
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take();
        let send = async {
            if let Some(mut stdin) = stdin {
                stdin
                    .write_all(input.unwrap_or_default().as_bytes())
                    .await?;
                stdin.shutdown().await?;
            }
            Ok::<_, std::io::Error>(())
        };
        let (_, output) = tokio::try_join!(send, child.wait_with_output())?;
        if !output.status.success() {
            bail!("clipboard helper failed");
        }
        Ok(output.stdout)
    })
    .await
    .context("clipboard helper timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn helper_preserves_multiline_unicode_and_handles_pipe_backpressure() {
        let text = "clipboard\n项目\n".repeat(16_384);
        let output = run("/bin/cat", &[], Some(&text), DEADLINE).await.unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), text);
    }

    #[tokio::test]
    async fn helpers_report_failure_and_have_a_deadline() {
        assert!(run("/usr/bin/false", &[], None, DEADLINE).await.is_err());
        assert!(
            run("/nonexistent/nonaut-clipboard", &[], None, DEADLINE)
                .await
                .is_err()
        );
        assert!(
            run("/bin/sleep", &["5"], None, Duration::from_millis(20))
                .await
                .is_err()
        );
    }
}
