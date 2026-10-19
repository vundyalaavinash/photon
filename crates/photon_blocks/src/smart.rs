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
        r"Summary \[[^\]]*\][ 	]+[0-9]+ tests? run: ([0-9]+) passed(?: \([0-9]+ slow\))?(?:, ([0-9]+) failed)?",
    )
});
static NEXTEST_FAIL: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[ 	]*FAIL \[[^\]]*\](?: \([0-9]+/[0-9]+\))? .*?([^ 	]+)$"));
static PYTEST_SUMMARY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^=+ (.+) in [[0-9].]+s(?: \(.*\))? =+$"));
static PYTEST_FAILED: LazyLock<Regex> = LazyLock::new(|| regex(r"^FAILED ([^ 	]+)"));
static PYTEST_SECTION: LazyLock<Regex> = LazyLock::new(|| regex(r"^_{2,} (.+?) _{2,}$"));
static PYTEST_TRACEBACK: LazyLock<Regex> = LazyLock::new(|| regex(r"^([^ 	]+\.py):([0-9]+): "));
static COUNT: LazyLock<Regex> = LazyLock::new(|| regex(r"([0-9]+) (failed|passed)"));
static JEST_SUMMARY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[ 	]*Tests(?::[ 	]+|[ 	]{2,})(.*(?:failed|passed).*)$"));
static JEST_FAILURE: LazyLock<Regex> = LazyLock::new(|| regex(r"^[ 	]*● (.+ › .+)$"));
static VITEST_FAILURE: LazyLock<Regex> = LazyLock::new(|| regex(r"^[ 	]*FAIL[ 	]+[^ 	]+ > (.+)$"));
static GO_RESULT: LazyLock<Regex> = LazyLock::new(|| regex(r"^--- (FAIL|PASS|SKIP): ([^ 	]+) \("));
static GO_PACKAGE: LazyLock<Regex> = LazyLock::new(|| regex(r"^(?:ok|FAIL)\t"));
static GO_LOCATION: LazyLock<Regex> = LazyLock::new(|| regex(r"^[ 	]+([^ 	]+\.go):([0-9]+):"));
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
    })
}

fn recognise_go(lines: &[OutputLine]) -> Option<TestRun> {
    let (mut passed, mut failed, mut found) = (0, 0, false);
    let mut failures = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if GO_PACKAGE.is_match(&line.text) {
            found = true;
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
    found.then_some(TestRun {
        tool: TestTool::Go,
        passed,
        failed,
        failures,
    })
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
    fn plain_output_has_no_insight() {
        assert_eq!(recognise("echo", &lines("hello\nworld"), Some(0)), None);
    }

    #[test]
    fn oversized_output_has_no_insight() {
        let text = "test result: ok. 1 passed; 0 failed\n".repeat(MAX_INSIGHT_LINES + 1);
        assert_eq!(recognise("cargo test", &lines(&text), Some(0)), None);
    }
}
