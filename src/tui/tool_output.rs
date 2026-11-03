use std::time::{Duration, Instant};

use crate::tools::{ToolContext, ToolFailure};

mod summary;

pub(super) const ANIMATION_TICK: Duration = Duration::from_millis(80);

pub(super) struct RunningTool {
    pub message_index: usize,
    pub context: ToolContext,
    pub started_at: Instant,
    pub latest_line: String,
    pub line_count: usize,
}

#[derive(Clone)]
pub(super) struct ToolOutput {
    pub context: ToolContext,
    pub elapsed: Option<Duration>,
    pub success: bool,
    pub output: String,
    pub failure: Option<ToolFailure>,
}

pub(super) fn title(context: &ToolContext) -> String {
    let mut title = if !context.argv_display.is_empty() {
        context
            .argv_display
            .iter()
            .map(|argument| {
                if argument == "<shell-command>" {
                    argument.clone()
                } else {
                    shell_words::quote(argument).into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    } else if matches!(context.tool.as_str(), "exec" | "run_command") {
        "command".to_owned()
    } else {
        context.tool.clone()
    };
    if let Some(target) = &context.target {
        title.push_str(&format!(" · {target}"));
    }
    if let Some(cwd) = &context.cwd_display
        && cwd != "."
    {
        title.push_str(&format!(" · {cwd}"));
    }
    title
}

pub(super) fn duration(elapsed: Duration) -> String {
    if elapsed.as_secs() < 60 {
        format!("{}s", elapsed.as_secs())
    } else {
        format!("{}m {:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    }
}

fn completed_duration(elapsed: Duration) -> String {
    if elapsed.as_secs() < 60 {
        format!("{}.{:03}s", elapsed.as_secs(), elapsed.subsec_millis())
    } else {
        format!(
            "{}m {:02}.{:03}s",
            elapsed.as_secs() / 60,
            elapsed.as_secs() % 60,
            elapsed.subsec_millis()
        )
    }
}

pub(super) fn heading(mark: &str, title: &str, elapsed: Option<Duration>) -> String {
    let mut content = format!("{mark} {title}");
    if let Some(elapsed) = elapsed
        && elapsed >= Duration::from_secs(1)
    {
        content.push_str(&format!(" · {}", completed_duration(elapsed)));
    }
    content
}

impl RunningTool {
    pub fn content(&self, width: u16) -> String {
        let elapsed = self.started_at.elapsed();
        let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let frame = frames
            [(elapsed.as_millis() / ANIMATION_TICK.as_millis() % frames.len() as u128) as usize];
        let title = format!("{frame} Running {}", title(&self.context));
        let mut timing = String::new();
        if elapsed >= Duration::from_secs(1) {
            timing.push_str(&duration(elapsed));
            // The deadline becomes relevant as the tool approaches it.
            if let Some(timeout) = self.context.timeout_seconds
                && elapsed.as_secs_f64() >= timeout as f64 * 0.8
            {
                let remaining = Duration::from_secs(timeout).saturating_sub(elapsed);
                timing.push_str(&format!(" · {} left", duration(remaining)));
            }
            timing.push_str(" · ");
        }
        timing.push_str("Esc cancel");
        let padding =
            usize::from(width).saturating_sub(title.chars().count() + timing.chars().count());
        let mut content = if padding < 2 {
            format!("{title} · {timing}")
        } else {
            format!("{title}{}{timing}", " ".repeat(padding))
        };
        if !self.latest_line.is_empty() {
            content.push_str("\n  ");
            content.push_str(&short_line(&self.latest_line).0);
        }
        content
    }

    pub fn stopped_content(&self, status: &str) -> String {
        format!(
            "{} · {}",
            heading("✗", &title(&self.context), Some(self.started_at.elapsed())),
            status.to_lowercase()
        )
    }
}

impl ToolOutput {
    pub fn content(&self) -> String {
        let mark = if self.success { "✓" } else { "✗" };
        let mut content = heading(mark, &title(&self.context), self.elapsed);
        if let Some(failure) = &self.failure {
            if let Some(code) = failure.exit_code {
                content.push_str(&format!(" · exit {code}"));
            }
            if failure.reason == "timeout" {
                content.push_str("\n  ");
                if let Some(seconds) = self.context.timeout_seconds {
                    content.push_str(&format!(
                        "Timed out after {}. Partial results only.",
                        duration(Duration::from_secs(seconds))
                    ));
                } else {
                    content.push_str("Timed out. Partial results only.");
                }
            } else if let Some(summary) = failure_summary(failure) {
                content.push_str("\n  ");
                content.push_str(summary);
            }
        }
        let detail = if self.success {
            success_preview(
                &self.output,
                matches!(self.context.tool.as_str(), "run_command" | "exec"),
            )
        } else {
            let output = if self
                .failure
                .as_ref()
                .is_some_and(|failure| failure.reason == "timeout")
            {
                self.output
                    .split_once("\nPartial output:\n")
                    .map_or(self.output.as_str(), |(_, partial)| partial)
            } else {
                &self.output
            };
            preview(output, 10)
        };
        if !detail.is_empty() {
            content.push('\n');
            content.push_str(&detail);
        }
        content
    }

    pub fn details(&self) -> String {
        let mut lines = vec![
            format!("Tool: {}", self.context.tool),
            format!("Execution: {}", title(&self.context)),
            format!(
                "Status: {}",
                if self.success { "succeeded" } else { "failed" }
            ),
        ];
        if let Some(cwd) = &self.context.cwd_display {
            lines.push(format!("Directory: {cwd}"));
        }
        if let Some(code) = self.failure.as_ref().and_then(|failure| failure.exit_code) {
            lines.push(format!("Exit code: {code}"));
        } else if self.success && matches!(self.context.tool.as_str(), "exec" | "run_command") {
            lines.push("Exit code: 0".to_owned());
        }
        if let Some(elapsed) = self.elapsed {
            lines.push(format!(
                "Duration: {}.{:03}s",
                elapsed.as_secs(),
                elapsed.subsec_millis()
            ));
        }
        if let Some(timeout) = self.context.timeout_seconds {
            lines.push(format!(
                "Timeout: {}",
                duration(Duration::from_secs(timeout))
            ));
        }
        if let Some(policy) = &self.context.policy_digest {
            lines.push(format!("Sandbox policy: {policy}"));
        }
        lines.push(String::new());
        lines.push(if self.output.trim().is_empty() {
            "(no output)".to_owned()
        } else {
            self.output.trim_end().to_owned()
        });
        lines.join("\n")
    }
}

fn failure_summary(failure: &ToolFailure) -> Option<&'static str> {
    match failure.reason {
        "sandbox_denied" | "workspace_denied" => Some("Sandbox denied this operation."),
        "timeout" => Some("Timed out."),
        "tls_error" => Some("TLS setup or certificate verification failed."),
        "network_error" => Some("Network connection or download failed."),
        "command_not_found" => Some("Command not found."),
        "command_not_executable" => Some("Command could not be started."),
        _ => None,
    }
}

fn short_line(line: &str) -> (String, bool) {
    let mut characters = line.chars();
    let mut shortened = characters.by_ref().take(240).collect::<String>();
    let clipped = characters.next().is_some();
    if clipped {
        shortened.push('…');
    }
    (shortened, clipped)
}

pub(super) fn preview(output: &str, visible: usize) -> String {
    excerpt(output, visible, true)
}

fn success_preview(output: &str, summarize_tests: bool) -> String {
    let clean = summary::clean(output);
    let warnings = summary::warnings(&clean);
    let report = if summarize_tests {
        summary::tests(&clean)
    } else {
        summary::TestReport::Absent
    };
    let mut rows = match report {
        summary::TestReport::Complete(tests) => vec![format!(
            "  {tests}{}",
            if warnings.labels.is_empty() {
                " · /details"
            } else {
                ""
            }
        )],
        summary::TestReport::Incomplete => {
            vec!["  Test totals unavailable: incomplete report · /details".to_owned()]
        }
        summary::TestReport::Absent => {
            if warnings.labels.is_empty() {
                return preview(&clean, 3);
            }
            let body = clean
                .lines()
                .enumerate()
                .filter_map(|(index, line)| (!warnings.lines.contains(&index)).then_some(line))
                .collect::<Vec<_>>()
                .join("\n");
            let detail = excerpt(&body, 2, false);
            if detail.is_empty() {
                Vec::new()
            } else {
                vec![detail]
            }
        }
    };
    if let Some(warning) = warnings.row() {
        rows.push(warning);
    }
    rows.join("\n")
}

fn excerpt(output: &str, visible: usize, details_hint: bool) -> String {
    let lines = output.trim_end().lines().collect::<Vec<_>>();
    let mut selected = if lines.len() <= visible {
        (0..lines.len()).collect::<Vec<_>>()
    } else {
        (0..visible / 3)
            .chain(lines.len() - (visible - visible / 3)..lines.len())
            .collect::<Vec<_>>()
    };
    selected.sort_unstable();
    let hidden = lines.len() - selected.len();
    let mut clipped = false;
    let mut result = Vec::new();
    let mut previous = None;
    for index in selected {
        if previous.is_some_and(|previous| index > previous + 1) {
            result.push("  …".to_owned());
        }
        let (line, shortened) = short_line(lines[index]);
        clipped |= shortened;
        result.push(format!("  {line}"));
        previous = Some(index);
    }
    if details_hint && hidden > 0 {
        result.push(format!("  … {hidden} more lines · /details"));
    } else if details_hint && clipped {
        result.push("  … /details for full output".to_owned());
    }
    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(tool: &str, output: &str, elapsed: Duration) -> ToolOutput {
        ToolOutput {
            context: ToolContext::minimal("call-1", tool),
            elapsed: Some(elapsed),
            success: true,
            output: output.to_owned(),
            failure: None,
        }
    }

    #[test]
    fn silent_successes_are_one_row_for_commands_and_other_tools() {
        let mut command = result("run_command", "", Duration::from_millis(999));
        command.context.argv_display = vec!["prettier".into(), "--write".into(), ".".into()];
        assert_eq!(command.content(), "✓ prettier --write .");
        assert_eq!(
            result("sync_service", " \n", Duration::ZERO).content(),
            "✓ sync_service"
        );
        assert!(command.details().contains("Exit code: 0"));
        assert!(command.details().ends_with("(no output)"));
    }

    #[test]
    fn timers_start_at_one_second_and_use_readable_units() {
        let mut output = result("fetch_report", "", Duration::from_secs(1));
        assert_eq!(output.content(), "✓ fetch_report · 1.000s");
        output.elapsed = Some(Duration::from_millis(1_400));
        assert_eq!(output.content(), "✓ fetch_report · 1.400s");
        output.elapsed = Some(Duration::from_secs(72));
        assert_eq!(output.content(), "✓ fetch_report · 1m 12.000s");
        output.elapsed = None;
        assert_eq!(output.content(), "✓ fetch_report");
    }

    #[test]
    fn live_timers_are_stable_within_each_second_and_completion_keeps_milliseconds() {
        for millis in [3_000, 3_247, 3_999] {
            assert_eq!(duration(Duration::from_millis(millis)), "3s");
        }
        assert_eq!(duration(Duration::from_millis(72_345)), "1m 12s");
        let mut output = result("fetch_report", "", Duration::from_millis(3_247));
        assert_eq!(output.content(), "✓ fetch_report · 3.247s");
        assert!(output.details().contains("Duration: 3.247s"));
        output.elapsed = Some(Duration::from_millis(72_345));
        assert_eq!(output.content(), "✓ fetch_report · 1m 12.345s");
    }

    #[test]
    fn running_tools_show_cancel_and_only_relevant_timing() {
        let mut running = RunningTool {
            message_index: 0,
            context: ToolContext::minimal("call-1", "fetch_report"),
            started_at: Instant::now(),
            latest_line: String::new(),
            line_count: 0,
        };
        running.context.timeout_seconds = Some(120);
        let instant = running.content(80);
        assert!(instant.contains("Running fetch_report"));
        assert!(instant.contains("Esc cancel"));
        assert!(!instant.contains("0.0s"));
        assert!(!instant.contains("left"));

        running.started_at = Instant::now() - Duration::from_secs(2);
        running.latest_line = "Downloading report".into();
        let active = running.content(30);
        assert!(active.contains("s · Esc cancel"));
        assert!(active.contains("Downloading report"));
        assert!(!active.contains("left"));

        running.started_at = Instant::now() - Duration::from_secs(100);
        assert!(running.content(80).contains("left"));
    }

    #[test]
    fn titles_show_targets_and_only_nondefault_directories() {
        let mut context = ToolContext::minimal("call-1", "upload_artifact");
        context.target = Some("report.pdf".into());
        context.cwd_display = Some(".".into());
        assert_eq!(title(&context), "upload_artifact · report.pdf");
        context.cwd_display = Some("reports".into());
        assert_eq!(title(&context), "upload_artifact · report.pdf · reports");
        context.target = None;
        context.argv_display = vec!["task".into(), "--label".into(), "two words".into()];
        assert_eq!(title(&context), "task --label 'two words' · reports");
    }

    #[test]
    fn compact_success_keeps_warnings_and_full_output_in_details() {
        let text = "starting\nworking\nWARNING: using cached data\nworking\n42 records exported\n";
        let mut output = result("export_records", text, Duration::from_secs(2));
        output.context.cwd_display = Some("reports".into());
        output.context.timeout_seconds = Some(120);
        output.context.policy_digest = Some("sha256:example".into());
        let card = output.content();
        assert!(card.contains("Warning: using cached data"));
        assert!(card.contains("42 records exported"));
        assert_eq!(card.matches("/details").count(), 1);
        let details = output.details();
        assert!(details.contains(text.trim_end()));
        assert!(details.contains("Directory: reports"));
        assert!(details.contains("Duration: 2.000s"));
        assert!(details.contains("Timeout: 2m 00s"));
        assert!(details.contains("Sandbox policy: sha256:example"));
        assert!(!details.contains("Exit code: 0"));
    }

    #[test]
    fn test_results_precede_grouped_warnings_without_repeating_the_raw_transcript() {
        let text = concat!(
            "warning: output of `xcrun` while finding MacOSX.sdk\n",
            "warning: linker stderr: couldn't create cache file '/tmp/xcrun_db-one' (errno=Operation not permitted)\n",
            "running 2 tests\n",
            "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s\n",
            "running 24 tests\n",
            "test result: ok. 0 passed; 0 failed; 24 ignored; 0 measured; 0 filtered out; finished in 0.0s\n",
        );
        let output = result("run_command", text, Duration::from_millis(2_500));
        let card = output.content();
        assert!(card.contains("  2 passed · 24 ignored\n  Warning:"));
        assert_eq!(card.matches("Warning:").count(), 1);
        assert!(!card.contains("0 passed"));
        assert!(!card.contains("xcrun_db-one"));
        assert!(output.details().contains(text.trim_end()));
    }

    #[test]
    fn timeout_cards_label_partial_results_and_the_configured_deadline() {
        let mut output = result(
            "run_command",
            "tool timed out after 1 seconds\nPartial output:\ncompiling dependencies",
            Duration::from_secs(1),
        );
        output.context.timeout_seconds = Some(1);
        output.success = false;
        output.failure = Some(ToolFailure {
            reason: "timeout",
            exit_code: None,
        });
        let card = output.content();
        assert!(card.contains("Timed out after 1s. Partial results only."));
        assert!(card.contains("compiling dependencies"));
        assert!(!card.contains("tool timed out after 1 seconds"));
        assert!(output.details().contains("Partial output:"));
    }

    #[test]
    fn failures_keep_exit_codes_and_diagnostics_at_both_ends() {
        let text = (1..=20)
            .map(|index| format!("diagnostic {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut output = result("run_command", &text, Duration::from_millis(100));
        output.success = false;
        output.failure = Some(ToolFailure {
            reason: "process_exit",
            exit_code: Some(7),
        });
        let card = output.content();
        assert!(card.starts_with("✗ command · exit 7\n"));
        assert!(card.contains("diagnostic 1\n"));
        assert!(card.contains("diagnostic 20"));
        assert!(!card.contains("diagnostic 10\n"));
        assert!(card.contains("10 more lines · /details"));
        assert!(output.details().contains("Exit code: 7"));
    }

    #[test]
    fn sandbox_denials_and_timeouts_keep_the_actual_error() {
        let mut output = result(
            "read_file",
            "Read denied at /private/report.pdf",
            Duration::ZERO,
        );
        output.success = false;
        output.failure = Some(ToolFailure {
            reason: "sandbox_denied",
            exit_code: None,
        });
        assert!(output.content().contains("Sandbox denied this operation."));
        assert!(
            output
                .content()
                .contains("Read denied at /private/report.pdf")
        );
        output.failure = Some(ToolFailure {
            reason: "timeout",
            exit_code: None,
        });
        output.output = "partial response".into();
        assert!(
            output
                .content()
                .contains("Timed out. Partial results only.")
        );
        assert!(output.content().contains("partial response"));
    }

    #[test]
    fn shortened_lines_offer_details_without_changing_the_result() {
        let text = "界".repeat(300);
        let output = result("query_service", &text, Duration::ZERO);
        assert!(output.content().contains("… /details for full output"));
        assert!(!output.content().contains(&text));
        assert!(output.details().ends_with(&text));
    }
}
