use std::path::{Path, PathBuf};

use tokio::process::Command;

const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Default)]
pub(super) struct Snapshot {
    pub diff: String,
    pub file_count: usize,
    pub error: Option<String>,
    /// Summary refreshes only update the badge. A full refresh also replaces
    /// the rendered diff.
    pub full: bool,
}

#[derive(Debug)]
struct StatusEntry {
    path: PathBuf,
    untracked: bool,
}

pub(super) async fn load(workspace: &Path) -> Snapshot {
    let status = match git(
        workspace,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            return Snapshot {
                error: Some(error),
                full: true,
                ..Snapshot::default()
            };
        }
    };
    let entries = parse_status(&status);
    if entries.is_empty() {
        return Snapshot::default();
    }

    let tracked = match git(
        workspace,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "HEAD",
            "--",
        ],
    )
    .await
    {
        Ok(output) => output,
        // An unborn repository has no HEAD. In that case, combine the staged
        // diff against the empty tree with the unstaged diff.
        Err(_) => {
            let mut output = git(
                workspace,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--cached",
                    "--",
                ],
            )
            .await
            .unwrap_or_default();
            if let Ok(unstaged) = git(
                workspace,
                &["diff", "--no-color", "--no-ext-diff", "--no-textconv", "--"],
            )
            .await
            {
                output.extend_from_slice(&unstaged);
            }
            output
        }
    };

    let mut diff = tracked;
    for entry in entries.iter().filter(|entry| entry.untracked) {
        let mut command = Command::new("git");
        command
            .args([
                "diff",
                "--no-index",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--",
            ])
            .arg("/dev/null")
            .arg(&entry.path)
            .current_dir(workspace)
            .env("GIT_OPTIONAL_LOCKS", "0");
        if let Ok(output) = command.output().await {
            // `git diff --no-index` uses exit code 1 when files differ.
            if output.status.success() || output.status.code() == Some(1) {
                diff.extend_from_slice(&output.stdout);
            }
        }
    }

    let truncated = diff.len() > MAX_DIFF_BYTES;
    diff.truncate(MAX_DIFF_BYTES);
    let mut diff = String::from_utf8_lossy(&diff).into_owned();
    if truncated {
        diff.push_str(
            "\n\nDiff truncated at 2 MiB. Use git diff in a terminal for the remainder.\n",
        );
    }
    Snapshot {
        diff,
        file_count: entries.len(),
        error: None,
        full: true,
    }
}

/// Count changes without reading and diffing their contents. The agent view
/// only needs the Review badge, so the more expensive full diff is deferred
/// until that view is opened.
pub(super) async fn load_summary(workspace: &Path) -> Snapshot {
    match git(
        workspace,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await
    {
        Ok(status) => Snapshot {
            file_count: parse_status(&status).len(),
            ..Snapshot::default()
        },
        Err(error) => Snapshot {
            error: Some(error),
            ..Snapshot::default()
        },
    }
}

async fn git(workspace: &Path, arguments: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(workspace)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .await
        .map_err(|error| format!("could not run git: {error}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(if message.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            message
        })
    }
}

fn parse_status(output: &[u8]) -> Vec<StatusEntry> {
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut entries = Vec::new();
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let status = &field[..2];
        let path = PathBuf::from(String::from_utf8_lossy(&field[3..]).into_owned());
        let renamed_or_copied = status.iter().any(|byte| matches!(*byte, b'R' | b'C'));
        if renamed_or_copied {
            // Porcelain v1 -z follows a rename destination with its source.
            let _ = fields.next();
        }
        entries.push(StatusEntry {
            path,
            untracked: status == b"??",
        });
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn porcelain_parser_counts_renames_once_and_finds_untracked_files() {
        let entries = parse_status(b" M tracked.rs\0?? new.rs\0R  renamed.rs\0old.rs\0");
        assert_eq!(entries.len(), 3);
        assert!(!entries[0].untracked);
        assert!(entries[1].untracked);
        assert_eq!(entries[2].path, PathBuf::from("renamed.rs"));
    }

    #[tokio::test]
    async fn snapshot_includes_untracked_file_contents() {
        let workspace = std::env::temp_dir().join(format!(
            "nonaut-changes-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&workspace).unwrap();
        let initialized = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(&workspace)
            .status()
            .unwrap();
        assert!(initialized.success());
        fs::write(workspace.join("new.txt"), "visible untracked contents\n").unwrap();

        let snapshot = load(&workspace).await;

        assert_eq!(snapshot.file_count, 1);
        assert!(snapshot.error.is_none());
        assert!(snapshot.diff.contains("visible untracked contents"));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn summary_counts_changes_without_building_a_diff() {
        let workspace = std::env::temp_dir().join(format!(
            "nonaut-changes-summary-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&workspace).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("init")
                .arg("--quiet")
                .current_dir(&workspace)
                .status()
                .unwrap()
                .success()
        );
        fs::write(workspace.join("new.txt"), "not loaded into the summary\n").unwrap();

        let snapshot = load_summary(&workspace).await;

        assert_eq!(snapshot.file_count, 1);
        assert!(snapshot.diff.is_empty());
        assert!(!snapshot.full);
        assert!(snapshot.error.is_none());
        fs::remove_dir_all(workspace).unwrap();
    }
}
