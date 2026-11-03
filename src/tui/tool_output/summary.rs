//! Optional adapters for complete, explicit test reports. Unknown output stays raw.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

use regex::Regex;

static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap());
static RUNNING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^running (\d+) tests?$").unwrap());
static LIBTEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out; finished in [\d.]+s$").unwrap()
});
static PYTEST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^=+\s+(.+?) in [\d.]+s(?: \([\d:]+\))?\s+=+$").unwrap());
static VITEST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Tests\s+(.+?)\s*\((\d+)\)$").unwrap());

#[derive(Default)]
struct Counts(BTreeMap<&'static str, u64>);

impl Counts {
    fn add(&mut self, label: &'static str, count: u64) -> Option<()> {
        let current = self.0.entry(label).or_default();
        *current = current.checked_add(count)?;
        Some(())
    }

    fn merge(&mut self, other: Counts) -> Option<()> {
        for (label, count) in other.0 {
            self.add(label, count)?;
        }
        Some(())
    }

    fn total(&self, labels: &[&str]) -> Option<u64> {
        labels.iter().try_fold(0_u64, |sum, label| {
            sum.checked_add(*self.0.get(label).unwrap_or(&0))
        })
    }

    fn text(&self) -> String {
        let mut parts = vec![format!("{} passed", self.0.get("passed").unwrap_or(&0))];
        for label in [
            "failed",
            "ignored",
            "skipped",
            "todo",
            "xfailed",
            "xpassed",
            "errors",
            "measured",
            "filtered",
            "deselected",
        ] {
            if let Some(count) = self.0.get(label).filter(|&&count| count > 0) {
                let label = if label == "errors" && *count == 1 {
                    "error"
                } else {
                    label
                };
                parts.push(format!("{count} {label}"));
            }
        }
        parts.join(" · ")
    }
}

pub(super) enum TestReport {
    Absent,
    Incomplete,
    Complete(String),
}

enum Parsed {
    Absent,
    Incomplete,
    Complete(Counts),
}

pub(super) fn clean(output: &str) -> String {
    ANSI.replace_all(output, "").into_owned()
}

pub(super) fn tests(output: &str) -> TestReport {
    let output = clean(output);
    let lines = output.lines().map(str::trim).collect::<Vec<_>>();
    let mut combined = Counts::default();
    let mut found = false;
    for report in [libtest(&lines), pytest(&lines), javascript(&lines)] {
        match report {
            Parsed::Absent => {}
            Parsed::Incomplete => return TestReport::Incomplete,
            Parsed::Complete(counts) => {
                if combined.merge(counts).is_none() {
                    return TestReport::Incomplete;
                }
                found = true;
            }
        }
    }
    if !found {
        TestReport::Absent
    } else if output.contains("[output truncated by Nonaut:") {
        TestReport::Incomplete
    } else {
        TestReport::Complete(combined.text())
    }
}

fn libtest(lines: &[&str]) -> Parsed {
    let mut counts = Counts::default();
    let mut pending = None;
    let mut found = false;
    for line in lines {
        if let Some(run) = RUNNING.captures(line) {
            if pending.is_some() {
                return Parsed::Incomplete;
            }
            let Ok(count) = run[1].parse::<u64>() else {
                return Parsed::Incomplete;
            };
            pending = Some(count);
            found = true;
        } else if line.starts_with("test result:") {
            let Some(report) = LIBTEST.captures(line) else {
                return Parsed::Incomplete;
            };
            let Some(expected) = pending.take() else {
                return Parsed::Incomplete;
            };
            let mut section = Counts::default();
            for (index, label) in ["passed", "failed", "ignored", "measured", "filtered"]
                .into_iter()
                .enumerate()
            {
                let Ok(count) = report[index + 1].parse::<u64>() else {
                    return Parsed::Incomplete;
                };
                if section.add(label, count).is_none() {
                    return Parsed::Incomplete;
                }
            }
            if section.total(&["passed", "failed", "ignored", "measured"]) != Some(expected)
                || counts.merge(section).is_none()
            {
                return Parsed::Incomplete;
            }
        }
    }
    if pending.is_some() {
        Parsed::Incomplete
    } else if found {
        Parsed::Complete(counts)
    } else {
        Parsed::Absent
    }
}

fn field(label: &str) -> Option<&'static str> {
    match label {
        "passed" => Some("passed"),
        "failed" => Some("failed"),
        "skipped" => Some("skipped"),
        "todo" => Some("todo"),
        "xfailed" => Some("xfailed"),
        "xpassed" => Some("xpassed"),
        "deselected" => Some("deselected"),
        "error" | "errors" => Some("errors"),
        "total" => Some("total"),
        "warning" | "warnings" => Some("warnings"),
        _ => None,
    }
}

fn fields(text: &str) -> Option<Counts> {
    let mut counts = Counts::default();
    for part in text.split([',', '|']) {
        let mut words = part.split_whitespace();
        let count = words.next()?.parse().ok()?;
        let label = field(words.next()?)?;
        if words.next().is_some() || counts.0.contains_key(label) {
            return None;
        }
        if label != "warnings" {
            counts.add(label, count)?;
        }
    }
    Some(counts)
}

fn pytest(lines: &[&str]) -> Parsed {
    let mut counts = Counts::default();
    let mut found = false;
    for line in lines {
        let Some(report) = PYTEST.captures(line) else {
            continue;
        };
        let section = if &report[1] == "no tests ran" {
            Some(Counts::default())
        } else {
            fields(&report[1])
        };
        let Some(section) = section else {
            return Parsed::Incomplete;
        };
        if section.0.contains_key("total") || counts.merge(section).is_none() {
            return Parsed::Incomplete;
        }
        found = true;
    }
    if found {
        Parsed::Complete(counts)
    } else {
        Parsed::Absent
    }
}

fn javascript(lines: &[&str]) -> Parsed {
    let mut counts = Counts::default();
    let mut found = false;
    for line in lines {
        let section = if let Some(text) = line.strip_prefix("Tests:") {
            fields(text)
        } else if let Some(report) = VITEST.captures(line) {
            fields(&report[1]).and_then(|mut section| {
                section.add("total", report[2].parse().ok()?)?;
                Some(section)
            })
        } else {
            continue;
        };
        let Some(mut section) = section else {
            return Parsed::Incomplete;
        };
        let total = section.0.remove("total");
        if total.is_none()
            || section.total(&["passed", "failed", "skipped", "todo"]) != total
            || counts.merge(section).is_none()
        {
            return Parsed::Incomplete;
        }
        found = true;
    }
    if found {
        Parsed::Complete(counts)
    } else {
        Parsed::Absent
    }
}

#[derive(Default)]
pub(super) struct Warnings {
    pub labels: Vec<String>,
    pub lines: BTreeSet<usize>,
}

impl Warnings {
    pub fn row(&self) -> Option<String> {
        match self.labels.as_slice() {
            [] => None,
            [label] => {
                let mut short = label.chars().take(100).collect::<String>();
                if label.chars().count() > 100 {
                    short.push('…');
                }
                Some(format!("  Warning: {short} · /details"))
            }
            labels => Some(format!("  {} warning groups · /details", labels.len())),
        }
    }
}

fn warning_message(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    for prefix in ["warning:", "warning ", "warn:", "[warn]", "[warning]"] {
        if lower.starts_with(prefix) {
            return Some(trimmed[prefix.len()..].trim());
        }
    }
    lower
        .find(": warning:")
        .map(|index| trimmed[index + ": warning:".len()..].trim())
}

pub(super) fn warnings(output: &str) -> Warnings {
    let lines = output.lines().collect::<Vec<_>>();
    let mut warnings = Warnings::default();
    let mut index = 0;
    while index < lines.len() {
        let Some(message) = warning_message(lines[index]) else {
            index += 1;
            continue;
        };
        let mut end = index + 1;
        while end < lines.len()
            && !lines[end].trim().is_empty()
            && lines[end].starts_with(char::is_whitespace)
            && warning_message(lines[end]).is_none()
        {
            end += 1;
        }
        let block = lines[index..end].join("\n");
        let label = if block.contains("xcrun_db")
            || (message.contains("`xcrun`") && output.contains("xcrun_db"))
        {
            "SDK cache warning".to_owned()
        } else {
            message.to_owned()
        };
        if !warnings.labels.contains(&label) {
            warnings.labels.push(label);
        }
        warnings.lines.extend(index..end);
        index = end;
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete(output: &str) -> String {
        match tests(output) {
            TestReport::Complete(text) => text,
            _ => panic!("missing complete report"),
        }
    }

    #[test]
    fn aggregates_all_libtest_sections_including_ignored_tests() {
        let output = "running 160 tests\ntest result: ok. 160 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s\nrunning 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.0s\nrunning 24 tests\ntest result: ok. 0 passed; 0 failed; 24 ignored; 0 measured; 0 filtered out; finished in 0.0s";
        assert_eq!(complete(output), "162 passed · 24 ignored");
        assert!(matches!(
            tests(&format!("{output}\nrunning 1 test")),
            TestReport::Incomplete
        ));
        assert!(matches!(
            tests(&format!(
                "[output truncated by Nonaut: middle omitted]\n{output}"
            )),
            TestReport::Incomplete
        ));
    }

    #[test]
    fn parses_python_and_javascript_reports_without_counting_suites_or_snapshots() {
        assert_eq!(
            complete("=== 3 passed, 1 skipped, 2 warnings in 0.12s ==="),
            "3 passed · 1 skipped"
        );
        assert_eq!(
            complete(
                "Test Suites: 1 passed, 1 total\nTests: 2 failed, 3 passed, 5 total\nSnapshots: 8 passed, 8 total"
            ),
            "3 passed · 2 failed"
        );
        assert_eq!(
            complete("Test Files  1 passed (1)\nTests  3 passed | 1 skipped (4)"),
            "3 passed · 1 skipped"
        );
        assert_eq!(
            complete("\x1b[32mTests: 3 passed, 3 total\x1b[0m"),
            "3 passed"
        );
    }

    #[test]
    fn refuses_unknown_malformed_or_incomplete_counts() {
        for output in [
            "running 2 tests\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s",
            "Tests: 2 passed, 4 total",
            "Tests: 9999999999999999999999 passed, 1 total",
        ] {
            assert!(matches!(tests(output), TestReport::Incomplete));
        }
        assert!(matches!(tests("exported 162 records"), TestReport::Absent));
    }

    #[test]
    fn groups_repeated_warnings_and_related_sdk_cache_fragments() {
        let output = "warning: output of `xcrun` while finding MacOSX.sdk\nwarning: linker stderr: cc: couldn't create cache file '/tmp/xcrun_db-one'\nwarning: linker stderr: cc: couldn't create cache file '/tmp/xcrun_db-two'\nfinished";
        let groups = warnings(output);
        assert_eq!(groups.labels, ["SDK cache warning"]);
        assert_eq!(groups.lines.len(), 3);
        assert!(!groups.lines.contains(&3));
        assert_eq!(
            warnings("warning: cached data\nwarning: cached data").labels,
            ["cached data"]
        );
    }
}
