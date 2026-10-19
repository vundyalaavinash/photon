use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputLine {
    pub absolute_line: i64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLocation {
    pub path: PathBuf,
    pub line: u32,
    pub column: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestTool {
    Cargo,
    Nextest,
    Pytest,
    Jest,
    Go,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestFailure {
    pub name: String,
    pub location: Option<SourceLocation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRun {
    pub tool: TestTool,
    pub passed: usize,
    pub failed: usize,
    pub failures: Vec<TestFailure>,
    // Failing Go packages, kept so a re-run can target them; empty for other tools.
    pub packages: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    Staged,
    Unstaged,
    Untracked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusFile {
    pub absolute_line: i64,
    pub path: String,
    pub state: FileState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitHash {
    pub absolute_line: i64,
    pub columns: Range<usize>,
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Insight {
    Tests(TestRun),
    Build { errors: Vec<SourceLocation> },
    GitStatus { files: Vec<StatusFile> },
    GitLog { commits: Vec<CommitHash> },
    Json,
    Table(Table),
}

pub const MAX_INSIGHT_LINES: usize = 20_000;
pub const MAX_INSIGHT_BYTES: usize = 5 * 1024 * 1024;

pub fn recognise(_command: &str, lines: &[OutputLine], _exit_code: Option<i32>) -> Option<Insight> {
    if lines.len() > MAX_INSIGHT_LINES
        || lines.iter().map(|line| line.text.len()).sum::<usize>() > MAX_INSIGHT_BYTES
    {
        return None;
    }
    recognise_tests(lines).map(Insight::Tests)
}

// Nextest echoes each failing test's own `test result:` line, so it must be tried before Cargo.
fn recognise_tests(lines: &[OutputLine]) -> Option<TestRun> {
    recognise_nextest(lines)
        .or_else(|| recognise_cargo(lines))
        .or_else(|| recognise_pytest(lines))
        .or_else(|| recognise_jest(lines))
        .or_else(|| recognise_go(lines))
}

fn regex(pattern: &str) -> Regex {
    // Patterns are compile-time constants covered by the tests below.
    Regex::new(pattern).unwrap_or_else(|error| panic!("invalid regex {pattern:?}: {error}"))
}

static CARGO_RESULT: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^test result: (?:ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed"));
static CARGO_FAILURE: LazyLock<Regex> = LazyLock::new(|| regex(r"^---- (.+) stdout ----$"));
static CARGO_PANIC: LazyLock<Regex> =
    LazyLock::new(|| regex(r"panicked at (.+?):([0-9]+):([0-9]+):?$"));
static NEXTEST_SUMMARY: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"Summary \[[^\]]*\][ \t]+[0-9]+ tests? run: ([0-9]+) passed(?: \([0-9]+ slow\))?(?:, ([0-9]+) failed)?",
    )
});
static NEXTEST_FAIL: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[ \t]*FAIL \[[^\]]*\](?: \([0-9]+/[0-9]+\))? .*?([^ \t]+)$"));
static PYTEST_SUMMARY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^=+ (.+) in [[0-9].]+s(?: \(.*\))? =+$"));
static PYTEST_FAILED: LazyLock<Regex> = LazyLock::new(|| regex(r"^FAILED ([^ \t]+)"));
static PYTEST_SECTION: LazyLock<Regex> = LazyLock::new(|| regex(r"^_{2,} (.+?) _{2,}$"));
static PYTEST_TRACEBACK: LazyLock<Regex> = LazyLock::new(|| regex(r"^([^ \t]+\.py):([0-9]+): "));
static COUNT: LazyLock<Regex> = LazyLock::new(|| regex(r"([0-9]+) (failed|passed)"));
static JEST_SUMMARY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[ \t]*Tests(?::[ \t]+|[ \t]{2,})(.*(?:failed|passed).*)$"));
static JEST_FAILURE: LazyLock<Regex> = LazyLock::new(|| regex(r"^[ \t]*● (.+ › .+)$"));
static VITEST_FAILURE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[ \t]*FAIL[ \t]+[^ \t]+ > (.+)$"));
static GO_RESULT: LazyLock<Regex> = LazyLock::new(|| regex(r"^--- (FAIL|PASS|SKIP): ([^ \t]+) \("));
static GO_PACKAGE: LazyLock<Regex> = LazyLock::new(|| regex(r"^(?:ok[ ]*|FAIL)\t"));
static GO_LOCATION: LazyLock<Regex> = LazyLock::new(|| regex(r"^[ \t]+([^ \t]+\.go):([0-9]+):"));
static GO_BOUNDARY: LazyLock<Regex> = LazyLock::new(|| regex(r"^(?:--- |FAIL|ok|PASS)"));

fn number(captures: &regex::Captures, group: usize) -> usize {
    captures
        .get(group)
        .and_then(|matched| matched.as_str().parse().ok())
        .unwrap_or(0)
}

fn location(path: &str, line: &str, column: Option<&str>) -> Option<SourceLocation> {
    Some(SourceLocation {
        path: PathBuf::from(path),
        line: line.parse().ok()?,
        column: column.and_then(|column| column.parse().ok()),
    })
}

fn push_unique(failures: &mut Vec<TestFailure>, failure: TestFailure) {
    if !failures
        .iter()
        .any(|existing| existing.name == failure.name)
    {
        failures.push(failure);
    }
}

fn counts(summary: &str) -> (usize, usize) {
    let (mut passed, mut failed) = (0, 0);
    for captures in COUNT.captures_iter(summary) {
        let count = number(&captures, 1);
        if &captures[2] == "passed" {
            passed = count;
        } else {
            failed = count;
        }
    }
    (passed, failed)
}

fn recognise_cargo(lines: &[OutputLine]) -> Option<TestRun> {
    let (mut passed, mut failed, mut found) = (0, 0, false);
    let mut failures = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(captures) = CARGO_RESULT.captures(&line.text) {
            found = true;
            passed += number(&captures, 1);
            failed += number(&captures, 2);
        } else if let Some(captures) = CARGO_FAILURE.captures(&line.text) {
            let panic_location = lines[index + 1..]
                .iter()
                .take_while(|next| !CARGO_FAILURE.is_match(&next.text) && next.text != "failures:")
                .find_map(|next| CARGO_PANIC.captures(&next.text))
                .and_then(|panic| location(&panic[1], &panic[2], Some(&panic[3])));
            push_unique(
                &mut failures,
                TestFailure {
                    name: captures[1].to_string(),
                    location: panic_location,
                },
            );
        }
    }
    found.then_some(TestRun {
        tool: TestTool::Cargo,
        passed,
        failed,
        failures,
        packages: Vec::new(),
    })
}

fn recognise_nextest(lines: &[OutputLine]) -> Option<TestRun> {
    let summary = lines
        .iter()
        .rev()
        .find_map(|line| NEXTEST_SUMMARY.captures(&line.text))?;
    let mut failures = Vec::new();
    for line in lines {
        if let Some(captures) = NEXTEST_FAIL.captures(&line.text) {
            push_unique(
                &mut failures,
                TestFailure {
                    name: captures[1].to_string(),
                    location: None,
                },
            );
        }
    }
    Some(TestRun {
        tool: TestTool::Nextest,
        passed: number(&summary, 1),
        failed: number(&summary, 2),
        failures,
        packages: Vec::new(),
    })
}

fn recognise_pytest(lines: &[OutputLine]) -> Option<TestRun> {
    let summary = lines
        .iter()
        .rev()
        .find_map(|line| PYTEST_SUMMARY.captures(&line.text))?;
    if !COUNT.is_match(&summary[1]) {
        return None;
    }
    let (passed, failed) = counts(&summary[1]);
    let mut failures = Vec::new();
    for line in lines {
        let Some(captures) = PYTEST_FAILED.captures(&line.text) else {
            continue;
        };
        let node_id = &captures[1];
        let short_name = node_id.rsplit("::").next().unwrap_or(node_id);
        let traceback = lines
            .iter()
            .position(|candidate| {
                PYTEST_SECTION
                    .captures(&candidate.text)
                    .is_some_and(|section| &section[1] == short_name)
            })
            .and_then(|start| {
                lines[start + 1..]
                    .iter()
                    .take_while(|next| !PYTEST_SECTION.is_match(&next.text))
                    .find_map(|next| PYTEST_TRACEBACK.captures(&next.text))
            })
            .and_then(|traceback| location(&traceback[1], &traceback[2], None));
        push_unique(
            &mut failures,
            TestFailure {
                name: node_id.to_string(),
                location: traceback,
            },
        );
    }
    Some(TestRun {
        tool: TestTool::Pytest,
        passed,
        failed,
        failures,
        packages: Vec::new(),
    })
}

// Vitest output is reported as `TestTool::Jest`: the two share a summary shape and the same actions.
fn recognise_jest(lines: &[OutputLine]) -> Option<TestRun> {
    let summary = lines
        .iter()
        .rev()
        .find_map(|line| JEST_SUMMARY.captures(&line.text))?;
    let (passed, failed) = counts(&summary[1]);
    let mut failures = Vec::new();
    for line in lines {
        if let Some(captures) = JEST_FAILURE
            .captures(&line.text)
            .or_else(|| VITEST_FAILURE.captures(&line.text))
        {
            push_unique(
                &mut failures,
                TestFailure {
                    name: captures[1].trim().to_string(),
                    location: None,
                },
            );
        }
    }
    Some(TestRun {
        tool: TestTool::Jest,
        passed,
        failed,
        failures,
        packages: Vec::new(),
    })
}

fn recognise_go(lines: &[OutputLine]) -> Option<TestRun> {
    let (mut passed, mut failed, mut found) = (0, 0, false);
    let (mut passed_packages, mut failed_packages) = (0, 0);
    let mut failures = Vec::new();
    let mut packages = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if GO_PACKAGE.is_match(&line.text) {
            found = true;
            if line.text.starts_with("ok") {
                passed_packages += 1;
            } else {
                failed_packages += 1;
                if let Some(package) = line.text.split('\t').nth(1) {
                    packages.push(package.to_string());
                }
            }
        }
        let Some(captures) = GO_RESULT.captures(&line.text) else {
            continue;
        };
        found = true;
        match &captures[1] {
            "PASS" => passed += 1,
            "FAIL" => {
                failed += 1;
                let name = captures[2].to_string();
                // Verbose runs print the location before the `--- FAIL` line, from the `=== RUN` line on.
                let start = lines[..index]
                    .iter()
                    .rposition(|earlier| earlier.text == format!("=== RUN   {name}"))
                    .unwrap_or(index);
                let end = lines[index + 1..]
                    .iter()
                    .position(|next| GO_BOUNDARY.is_match(&next.text))
                    .map_or(lines.len(), |offset| index + 1 + offset);
                let failure_location = lines[start..end]
                    .iter()
                    .find_map(|candidate| GO_LOCATION.captures(&candidate.text))
                    .and_then(|found| location(&found[1], &found[2], None));
                push_unique(
                    &mut failures,
                    TestFailure {
                        name,
                        location: failure_location,
                    },
                );
            }
            _ => {}
        }
    }
    // Plain `go test` prints no per-test lines for passing runs, so fall back to package granularity.
    if passed == 0 && failed == 0 {
        (passed, failed) = (passed_packages, failed_packages);
    }
    found.then_some(TestRun {
        tool: TestTool::Go,
        passed,
        failed,
        failures,
        packages,
    })
}

const UNSAFE_CHARACTERS: &[char] = &['|', ';', '&', '<', '>', '$', '`', '(', ')', '\n', '\'', '"'];

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn has_sequence(tokens: &[&str], sequence: &[&str]) -> bool {
    tokens
        .windows(sequence.len())
        .any(|window| window == sequence)
}

fn flags<'a>(tokens: &[&'a str]) -> Vec<&'a str> {
    tokens
        .iter()
        .copied()
        .filter(|token| token.starts_with('-'))
        .collect()
}

pub fn rerun_failed_command(command: &str, run: &TestRun) -> Option<String> {
    if run.failed == 0 || run.failures.is_empty() || command.contains(UNSAFE_CHARACTERS) {
        return None;
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let separator = tokens.iter().position(|token| *token == "--");
    let before_separator = &tokens[..separator.unwrap_or(tokens.len())];
    let names: Vec<&str> = run
        .failures
        .iter()
        .map(|failure| failure.name.as_str())
        .collect();
    let quoted_names = || {
        names
            .iter()
            .map(|name| shell_quote(name))
            .collect::<Vec<_>>()
    };

    match run.tool {
        TestTool::Cargo => {
            if !has_sequence(before_separator, &["cargo", "test"]) {
                return None;
            }
            let after_separator = separator.map_or(&[][..], |index| &tokens[index + 1..]);
            let mut parts: Vec<String> = before_separator.iter().map(|t| t.to_string()).collect();
            parts.push("--".into());
            parts.extend(quoted_names());
            parts.push("--exact".into());
            parts.extend(flags(after_separator).into_iter().map(String::from));
            Some(parts.join(" "))
        }
        TestTool::Nextest => {
            if !has_sequence(before_separator, &["cargo", "nextest", "run"])
                || tokens
                    .iter()
                    .any(|token| token.starts_with("-E") || token.starts_with("--filterset"))
            {
                return None;
            }
            let expression = names
                .iter()
                .map(|name| format!("test(={name})"))
                .collect::<Vec<_>>()
                .join(" | ");
            Some(format!(
                "{} -E {}",
                before_separator.join(" "),
                shell_quote(&expression)
            ))
        }
        TestTool::Pytest => {
            let position = tokens
                .iter()
                .position(|token| *token == "pytest" || token.ends_with("/pytest"))?;
            let mut parts: Vec<String> =
                tokens[..=position].iter().map(|t| t.to_string()).collect();
            parts.extend(flags(&tokens[position + 1..]).into_iter().map(String::from));
            parts.extend(quoted_names());
            Some(parts.join(" "))
        }
        TestTool::Jest => {
            let pattern = names
                .iter()
                .map(|name| regex::escape(&name.replace(" › ", " ").replace(" > ", " ")))
                .collect::<Vec<_>>()
                .join("|");
            let filter = format!("-t {}", shell_quote(&pattern));
            if tokens
                .iter()
                .any(|token| *token == "jest" || *token == "vitest")
            {
                Some(format!("{command} {filter}"))
            } else if matches!(tokens.first(), Some(&("npm" | "pnpm" | "yarn" | "bun"))) {
                let dashes = if separator.is_some() { "" } else { "-- " };
                Some(format!("{command} {dashes}{filter}"))
            } else {
                None
            }
        }
        TestTool::Go => {
            if !tokens.starts_with(&["go", "test"]) {
                return None;
            }
            let mut top_level: Vec<&str> = Vec::new();
            for name in &names {
                let top = name.split('/').next().unwrap_or(name);
                if !top_level.contains(&top) {
                    top_level.push(top);
                }
            }
            let pattern = top_level
                .iter()
                .map(|name| regex::escape(name))
                .collect::<Vec<_>>()
                .join("|");
            let mut parts: Vec<String> = vec!["go test".into()];
            parts.extend(
                flags(tokens.get(2..).unwrap_or_default())
                    .into_iter()
                    .map(String::from),
            );
            parts.push(format!("-run {}", shell_quote(&format!("^({pattern})$"))));
            parts.extend(run.packages.iter().map(|package| shell_quote(package)));
            Some(parts.join(" "))
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunDelta {
    pub new_failures: Vec<String>,
    pub fixed: Vec<String>,
}

pub fn compare_runs(previous: &TestRun, current: &TestRun) -> RunDelta {
    let failed_names = |run: &TestRun| -> BTreeSet<String> {
        run.failures
            .iter()
            .map(|failure| failure.name.clone())
            .collect()
    };
    let (before, now) = (failed_names(previous), failed_names(current));
    RunDelta {
        new_failures: now.difference(&before).cloned().collect(),
        fixed: before.difference(&now).cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<OutputLine> {
        text.lines()
            .enumerate()
            .map(|(index, text)| OutputLine {
                absolute_line: index as i64,
                text: text.to_string(),
            })
            .collect()
    }

    fn fixture(text: &str) -> Vec<OutputLine> {
        lines(text.strip_prefix("# hand-written\n").unwrap_or(text))
    }

    fn tests_of(text: &str) -> TestRun {
        match recognise("", &fixture(text), None) {
            Some(Insight::Tests(run)) => run,
            other => panic!("expected Tests, got {other:?}"),
        }
    }

    fn names(run: &TestRun) -> Vec<&str> {
        run.failures
            .iter()
            .map(|failure| failure.name.as_str())
            .collect()
    }

    fn location(path: &str, line: u32, column: Option<u32>) -> Option<SourceLocation> {
        Some(SourceLocation {
            path: PathBuf::from(path),
            line,
            column,
        })
    }

    #[test]
    fn cargo_failures_counted_and_located() {
        let run = tests_of(include_str!("../test_fixtures/smart/cargo_multi.txt"));
        assert_eq!(run.tool, TestTool::Cargo);
        assert_eq!((run.failed, run.passed), (2, 41));
        assert_eq!(
            names(&run),
            ["tests::rejects_empty", "tests::handles_unicode"]
        );
        assert_eq!(
            run.failures[0].location,
            location("crates/cli/src/main.rs", 356, Some(9))
        );
        assert_eq!(
            run.failures[1].location,
            location("crates/core/src/lib.rs", 88, Some(13))
        );

        let run = tests_of(include_str!("../test_fixtures/smart/cargo_fail.txt"));
        assert_eq!((run.failed, run.passed), (2, 2));
        assert_eq!(names(&run), ["bad_one", "bad_two"]);
        assert_eq!(
            run.failures[0].location,
            location("src/lib.rs", 3, Some(23))
        );
    }

    #[test]
    fn cargo_passing_run() {
        let run = tests_of(include_str!("../test_fixtures/smart/cargo_pass.txt"));
        assert_eq!(run.tool, TestTool::Cargo);
        assert_eq!(run.failed, 0);
        assert_eq!(run.passed, 2);
        assert!(run.failures.is_empty());
    }

    #[test]
    fn nextest_summary() {
        let run = tests_of(include_str!("../test_fixtures/smart/nextest_fail.txt"));
        assert_eq!(run.tool, TestTool::Nextest);
        assert_eq!((run.failed, run.passed), (2, 2));
        assert_eq!(names(&run), ["bad_two", "bad_one"]);
    }

    #[test]
    fn pytest_failures() {
        let run = tests_of(include_str!("../test_fixtures/smart/pytest_fail.txt"));
        assert_eq!(run.tool, TestTool::Pytest);
        assert_eq!((run.failed, run.passed), (2, 1));
        assert_eq!(names(&run), ["test_a.py::test_bad", "test_a.py::test_bad2"]);
        assert_eq!(run.failures[0].location, location("test_a.py", 2, None));
        assert_eq!(run.failures[1].location, location("test_a.py", 5, None));
    }

    #[test]
    fn jest_and_vitest_failures() {
        let run = tests_of(include_str!("../test_fixtures/smart/jest_fail.txt"));
        assert_eq!(run.tool, TestTool::Jest);
        assert_eq!((run.failed, run.passed), (2, 5));
        assert_eq!(
            names(&run),
            ["sum › subtracts numbers", "sum › handles zero"]
        );

        let run = tests_of(include_str!("../test_fixtures/smart/vitest_fail.txt"));
        assert_eq!(run.tool, TestTool::Jest);
        assert_eq!((run.failed, run.passed), (1, 2));
        assert_eq!(names(&run), ["sum > subtracts numbers"]);
    }

    #[test]
    fn go_test_failures() {
        let run = tests_of(include_str!("../test_fixtures/smart/go_fail.txt"));
        assert_eq!(run.tool, TestTool::Go);
        assert_eq!((run.failed, run.passed), (1, 0));
        assert_eq!(names(&run), ["TestBad"]);
        assert_eq!(run.failures[0].location, location("a_test.go", 5, None));

        let run = tests_of(include_str!("../test_fixtures/smart/go_verbose_fail.txt"));
        assert_eq!((run.failed, run.passed), (1, 1));
        assert_eq!(names(&run), ["TestBad"]);
        assert_eq!(run.failures[0].location, location("a_test.go", 5, None));
    }

    #[test]
    fn go_plain_passing_run_counts_packages() {
        let run = tests_of(include_str!("../test_fixtures/smart/go_plain_pass.txt"));
        assert_eq!(run.tool, TestTool::Go);
        assert_eq!((run.failed, run.passed), (0, 2));
        assert!(run.failures.is_empty());
    }

    #[test]
    fn plain_output_has_no_insight() {
        assert_eq!(recognise("echo", &lines("hello\nworld"), Some(0)), None);
    }

    #[test]
    fn oversized_output_has_no_insight() {
        let text = "test result: ok. 1 passed; 0 failed\n".repeat(MAX_INSIGHT_LINES + 1);
        assert_eq!(recognise("cargo test", &lines(&text), Some(0)), None);
    }

    fn run(tool: TestTool, names: &[&str]) -> TestRun {
        TestRun {
            tool,
            passed: 1,
            failed: names.len(),
            failures: names
                .iter()
                .map(|name| TestFailure {
                    name: name.to_string(),
                    location: None,
                })
                .collect(),
            packages: Vec::new(),
        }
    }

    #[test]
    fn rerun_cargo() {
        let run = run(TestTool::Cargo, &["tests::a", "tests::b"]);
        assert_eq!(
            rerun_failed_command("cargo test -p cli", &run).as_deref(),
            Some("cargo test -p cli -- 'tests::a' 'tests::b' --exact")
        );
    }

    #[test]
    fn rerun_cargo_keeps_flags_after_dashes() {
        let run = run(TestTool::Cargo, &["a"]);
        assert_eq!(
            rerun_failed_command("cargo test -- --nocapture foo", &run).as_deref(),
            Some("cargo test -- 'a' --exact --nocapture")
        );
    }

    #[test]
    fn rerun_nextest() {
        let run = run(TestTool::Nextest, &["a", "b"]);
        assert_eq!(
            rerun_failed_command("cargo nextest run -p x", &run).as_deref(),
            Some("cargo nextest run -p x -E 'test(=a) | test(=b)'")
        );
        assert_eq!(
            rerun_failed_command("cargo nextest run -E 'test(c)'", &run),
            None
        );
        assert_eq!(
            rerun_failed_command("cargo nextest run --filterset=test(c)", &run),
            None
        );
    }

    #[test]
    fn rerun_pytest() {
        let run = run(TestTool::Pytest, &["tests/t.py::test_a"]);
        assert_eq!(
            rerun_failed_command("python -m pytest -x tests/", &run).as_deref(),
            Some("python -m pytest -x 'tests/t.py::test_a'")
        );
    }

    #[test]
    fn rerun_jest_via_npm() {
        let run = run(TestTool::Jest, &["suite › name"]);
        assert_eq!(
            rerun_failed_command("npm test", &run).as_deref(),
            Some("npm test -- -t 'suite name'")
        );
        assert_eq!(
            rerun_failed_command("npx jest src", &run).as_deref(),
            Some("npx jest src -t 'suite name'")
        );
        let run = self::run(TestTool::Jest, &["sum > a.b", "x › y"]);
        assert_eq!(
            rerun_failed_command("pnpm vitest run", &run).as_deref(),
            Some("pnpm vitest run -t 'sum a\\.b|x y'")
        );
        assert_eq!(rerun_failed_command("make check", &run), None);
    }

    #[test]
    fn rerun_go() {
        let mut run = run(TestTool::Go, &["TestA", "TestA/sub"]);
        run.packages = vec!["example.com/pkg".to_string()];
        assert_eq!(
            rerun_failed_command("go test ./...", &run).as_deref(),
            Some("go test -run '^(TestA)$' 'example.com/pkg'")
        );
        assert_eq!(
            rerun_failed_command("go test -v ./...", &run).as_deref(),
            Some("go test -v -run '^(TestA)$' 'example.com/pkg'")
        );
        assert_eq!(rerun_failed_command("make test", &run), None);
    }

    #[test]
    fn go_run_keeps_failing_packages() {
        let run = tests_of(include_str!("../test_fixtures/smart/go_fail.txt"));
        assert_eq!(run.packages, ["demo"]);
    }

    #[test]
    fn rerun_quotes_names() {
        let run = run(TestTool::Cargo, &["it's"]);
        assert_eq!(
            rerun_failed_command("cargo test", &run).as_deref(),
            Some("cargo test -- 'it'\\''s' --exact")
        );
    }

    #[test]
    fn rerun_refuses_unsafe_commands() {
        let failing = run(TestTool::Cargo, &["a"]);
        for command in [
            "cargo test | tee log",
            "make test",
            "cargo test; echo",
            "cargo test 'x'",
            "cargo test $(x)",
        ] {
            assert_eq!(rerun_failed_command(command, &failing), None, "{command}");
        }
        let mut passing = run(TestTool::Cargo, &[]);
        passing.failed = 0;
        assert_eq!(rerun_failed_command("cargo test", &passing), None);
        let mut nameless = run(TestTool::Cargo, &[]);
        nameless.failed = 1;
        assert_eq!(rerun_failed_command("cargo test", &nameless), None);
    }

    #[test]
    fn compare_runs_reports_new_and_fixed() {
        let previous = run(TestTool::Cargo, &["b", "a", "kept"]);
        let current = run(TestTool::Cargo, &["kept", "z", "c"]);
        assert_eq!(
            compare_runs(&previous, &current),
            RunDelta {
                new_failures: vec!["c".to_string(), "z".to_string()],
                fixed: vec!["a".to_string(), "b".to_string()],
            }
        );
    }
}
