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
const MAX_TABLE_HEADER_CHARS: usize = 1_000;
pub const MAX_INSIGHT_BYTES: usize = 5 * 1024 * 1024;

pub fn recognise(command: &str, lines: &[OutputLine], _exit_code: Option<i32>) -> Option<Insight> {
    if lines.len() > MAX_INSIGHT_LINES
        || lines.iter().map(|line| line.text.len()).sum::<usize>() > MAX_INSIGHT_BYTES
    {
        return None;
    }
    if let Some(run) = recognise_tests(lines) {
        return Some(Insight::Tests(run));
    }
    let mut words = command.split_whitespace();
    let git_subcommand = (words.next() == Some("git"))
        .then(|| words.next())
        .flatten();
    recognise_build(lines)
        .or_else(|| match git_subcommand {
            Some("status") => recognise_git_status(lines),
            Some("log") => recognise_git_log(lines),
            _ => None,
        })
        .or_else(|| recognise_json(lines))
        .or_else(|| recognise_table(lines))
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

const UNSAFE_CHARACTERS: &[char] = &[
    '|', ';', '&', '<', '>', '$', '`', '(', ')', '\n', '\'', '"', '#', '\\',
];

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn has_sequence(tokens: &[&str], sequence: &[&str]) -> bool {
    tokens
        .windows(sequence.len())
        .any(|window| window == sequence)
}

fn known_flags<'a>(tokens: &[&'a str], booleans: &[&str]) -> Option<Vec<&'a str>> {
    let mut kept = Vec::new();
    for token in tokens
        .iter()
        .copied()
        .filter(|token| token.starts_with('-'))
    {
        if booleans.contains(&token) || token.contains('=') {
            kept.push(token);
        } else {
            return None;
        }
    }
    Some(kept)
}

const PYTEST_FLAGS: &[&str] = &["-v", "-vv", "-q", "-x", "-s", "-l", "--lf", "--ff"];
const GO_FLAGS: &[&str] = &["-v", "-race", "-short", "-failfast", "-cover", "-json"];

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
                || tokens.iter().any(|token| {
                    token.starts_with("-E")
                        || token.starts_with("--filterset")
                        || token.starts_with("--filter-expr")
                })
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
            parts.extend(
                known_flags(&tokens[position + 1..], PYTEST_FLAGS)?
                    .into_iter()
                    .map(String::from),
            );
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
            if tokens.first() == Some(&"npm") {
                let dashes = if separator.is_some() { "" } else { "-- " };
                Some(format!("{command} {dashes}{filter}"))
            } else if matches!(tokens.first(), Some(&("pnpm" | "yarn" | "bun"))) {
                Some(format!("{command} {filter}"))
            } else if tokens
                .iter()
                .any(|token| *token == "jest" || *token == "vitest")
            {
                Some(format!("{command} {filter}"))
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
            if tokens.iter().any(|token| token.starts_with("-run")) {
                return None;
            }
            let mut parts: Vec<String> = vec!["go test".into()];
            parts.extend(
                known_flags(tokens.get(2..).unwrap_or_default(), GO_FLAGS)?
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

static RUSTC_ERROR: LazyLock<Regex> = LazyLock::new(|| regex(r"^error(?:\[[A-Za-z0-9]+\])?:"));
static RUSTC_BOUNDARY: LazyLock<Regex> = LazyLock::new(|| regex(r"^(?:error|warning)"));
static RUSTC_ARROW: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[ \t]*--> (.+?):([0-9]+):([0-9]+)$"));
static TSC_ERROR: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^(.+?)\(([0-9]+),([0-9]+)\): error TS"));
static GCC_ERROR: LazyLock<Regex> = LazyLock::new(|| regex(r"^(.+?):([0-9]+):([0-9]+): error:"));
static STATUS_SECTION: LazyLock<Regex> = LazyLock::new(|| regex(r"^[A-Z][^\t]*:$"));
static STATUS_LONG_ENTRY: LazyLock<Regex> = LazyLock::new(|| regex(r"^\t([a-z][a-z ]*):[ ]+(.+)$"));
static STATUS_SHORT_ENTRY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^([ MTADRCU?!])([ MTADRCU?!]) (.+)$"));
static LOG_HASH: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^(?:commit )?([0-9a-f]{7,40})(?:[ \t]|$)"));
static LOCAL_URL: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"https?://(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\])(:[0-9]+)?(/[^ \t]*)?")
});
static ERROR_LINE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i-u)\b(error|failed|exception|panic(ked)?)\b"));
static NO_ERROR_LINE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i-u)\b(0|no) (errors?|failures?)\b"));

fn recognise_build(lines: &[OutputLine]) -> Option<Insight> {
    let mut errors = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let found = if RUSTC_ERROR.is_match(&line.text) {
            lines[index + 1..]
                .iter()
                .take_while(|next| !RUSTC_BOUNDARY.is_match(&next.text))
                .find_map(|next| RUSTC_ARROW.captures(&next.text))
        } else {
            TSC_ERROR
                .captures(&line.text)
                .or_else(|| GCC_ERROR.captures(&line.text))
        };
        if let Some(error) = found.and_then(|c| location(&c[1], &c[2], Some(&c[3]))) {
            errors.push(error);
        }
    }
    (!errors.is_empty()).then_some(Insight::Build { errors })
}

// Git quotes paths with special characters as C strings with octal escapes for non-ASCII bytes.
fn unquote_git_path(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return path.to_string();
    };
    let bytes = inner.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        index += 1;
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }
        let Some(&escaped) = bytes.get(index) else {
            decoded.push(byte);
            break;
        };
        index += 1;
        match escaped {
            b'0'..=b'7' => {
                let mut value = u32::from(escaped - b'0');
                for _ in 0..2 {
                    match bytes.get(index) {
                        Some(&digit @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(digit - b'0');
                            index += 1;
                        }
                        _ => break,
                    }
                }
                decoded.push(value as u8);
            }
            b'n' => decoded.push(b'\n'),
            b't' => decoded.push(b'\t'),
            b'a' => decoded.push(0x07),
            b'b' => decoded.push(0x08),
            b'f' => decoded.push(0x0c),
            b'v' => decoded.push(0x0b),
            b'r' => decoded.push(0x0d),
            other => decoded.push(other),
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn status_path(text: &str, renamed: bool) -> String {
    let target = match text.split_once(" -> ") {
        Some((_, new)) if renamed => new,
        _ => text,
    };
    unquote_git_path(target.trim())
}

fn recognise_git_status(lines: &[OutputLine]) -> Option<Insight> {
    let mut files = Vec::new();
    let mut section = None;
    for line in lines {
        let text = line.text.as_str();
        if STATUS_SECTION.is_match(text) {
            section = match text {
                "Changes to be committed:" => Some(FileState::Staged),
                "Changes not staged for commit:" | "Unmerged paths:" => Some(FileState::Unstaged),
                "Untracked files:" => Some(FileState::Untracked),
                _ => None,
            };
            continue;
        }
        let Some(state) = section else { continue };
        let entry = if state == FileState::Untracked {
            text.strip_prefix('\t').map(|path| (path, false))
        } else {
            STATUS_LONG_ENTRY.captures(text).map(|captures| {
                let renamed = matches!(&captures[1], "renamed" | "copied");
                (captures.get(2).map_or("", |path| path.as_str()), renamed)
            })
        };
        if let Some((path, renamed)) = entry {
            files.push(StatusFile {
                absolute_line: line.absolute_line,
                path: status_path(path, renamed),
                state,
            });
        }
    }
    if files.is_empty() {
        for line in lines {
            let Some(captures) = STATUS_SHORT_ENTRY.captures(&line.text) else {
                continue;
            };
            let (index, worktree) = (&captures[1], &captures[2]);
            let renamed = index == "R" || index == "C" || worktree == "R" || worktree == "C";
            let path = status_path(&captures[3], renamed);
            let mut states = Vec::new();
            if index == "?" && worktree == "?" {
                states.push(FileState::Untracked);
            } else if index != "!" {
                if index != " " {
                    states.push(FileState::Staged);
                }
                if worktree != " " {
                    states.push(FileState::Unstaged);
                }
            }
            files.extend(states.into_iter().map(|state| StatusFile {
                absolute_line: line.absolute_line,
                path: path.clone(),
                state,
            }));
        }
    }
    (!files.is_empty()).then_some(Insight::GitStatus { files })
}

fn recognise_git_log(lines: &[OutputLine]) -> Option<Insight> {
    let commits: Vec<CommitHash> = lines
        .iter()
        .filter_map(|line| {
            let hash = LOG_HASH.captures(&line.text)?.get(1)?;
            Some(CommitHash {
                absolute_line: line.absolute_line,
                columns: hash.start()..hash.end(),
                hash: hash.as_str().to_string(),
            })
        })
        .collect();
    (!commits.is_empty()).then_some(Insight::GitLog { commits })
}

fn recognise_json(lines: &[OutputLine]) -> Option<Insight> {
    let text = lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = text.trim();
    (trimmed.starts_with(['{', '[']) && serde_json::from_str::<serde_json::Value>(trimmed).is_ok())
        .then_some(Insight::Json)
}

fn recognise_table(lines: &[OutputLine]) -> Option<Insight> {
    let rows: Vec<Vec<char>> = lines
        .iter()
        .map(|line| line.text.trim_end().chars().collect())
        .collect();
    let first = rows.iter().position(|row| !row.is_empty())?;
    let last = rows.iter().rposition(|row| !row.is_empty())?;
    let rows = &rows[first..=last];
    if rows.len() < 3 {
        return None;
    }
    let header = &rows[0];
    if header.len() > MAX_TABLE_HEADER_CHARS {
        return None;
    }
    let is_space_at = |row: &Vec<char>, index: usize| row.get(index).is_none_or(|c| *c == ' ');
    let mut starts = vec![0];
    starts.extend((1..header.len().saturating_sub(1)).filter_map(|index| {
        let boundary = is_space_at(header, index)
            && header.get(index + 1).is_some_and(|c| *c != ' ')
            && rows.iter().all(|row| is_space_at(row, index));
        boundary.then_some(index + 1)
    }));
    if starts.len() < 2 {
        return None;
    }
    let cells = |row: &Vec<char>| -> Vec<String> {
        starts
            .iter()
            .enumerate()
            .map(|(column, &start)| {
                let end = starts.get(column + 1).map_or(row.len(), |next| next - 1);
                row.get(start..end.max(start).min(row.len()))
                    .map(|cell| cell.iter().collect::<String>().trim().to_string())
                    .unwrap_or_default()
            })
            .collect()
    };
    let headers = cells(header);
    if headers
        .iter()
        .any(|cell| cell.is_empty() || cell.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let rows: Vec<Vec<String>> = rows[1..]
        .iter()
        .map(cells)
        .filter(|row| row.iter().any(|cell| !cell.is_empty()))
        .collect();
    if rows.len() < 2 {
        return None;
    }
    // Prose lines can share a space column by chance; real table rows rarely leave most cells empty.
    if rows
        .iter()
        .any(|row| row.iter().filter(|cell| !cell.is_empty()).count() * 2 < row.len())
    {
        return None;
    }
    Some(Insight::Table(Table { headers, rows }))
}

pub fn find_local_url(line: &str) -> Option<String> {
    let captures = LOCAL_URL.captures_iter(line).find(|captures| {
        let Some(matched) = captures.get(0) else {
            return false;
        };
        let after_host = captures.get(2).or(captures.get(1)).map_or(0, |m| m.end());
        after_host != matched.end()
            || !line[after_host..]
                .starts_with(|c: char| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    })?;
    let matched = captures.get(0)?;
    let host = captures.get(1)?;
    if matches!(host.as_str(), "0.0.0.0" | "[::]") {
        let text = matched.as_str();
        let offset = host.start() - matched.start();
        return Some(format!(
            "{}localhost{}",
            &text[..offset],
            &text[offset + host.len()..]
        ));
    }
    Some(matched.as_str().to_string())
}

pub fn url_label(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest).to_string()
}

pub fn is_error_line(line: &str) -> bool {
    ERROR_LINE.is_match(line) && !NO_ERROR_LINE.is_match(line)
}

pub fn table_to_csv(table: &Table) -> String {
    let mut csv = String::new();
    for row in std::iter::once(&table.headers).chain(&table.rows) {
        let record: Vec<String> = row
            .iter()
            .map(|cell| {
                if cell.contains([',', '"', '\n', '\r']) {
                    format!("\"{}\"", cell.replace('"', "\"\""))
                } else {
                    cell.clone()
                }
            })
            .collect();
        csv.push_str(&record.join(","));
        csv.push('\n');
    }
    csv
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

    #[test]
    fn rerun_refuses_flags_with_unknown_values() {
        let pytest = run(TestTool::Pytest, &["t.py::a"]);
        assert_eq!(
            rerun_failed_command("pytest -k smoke tests/", &pytest),
            None
        );
        assert_eq!(
            rerun_failed_command("pytest -v --tb=short", &pytest).as_deref(),
            Some("pytest -v --tb=short 't.py::a'")
        );
        let mut go = run(TestTool::Go, &["TestA"]);
        go.packages = vec!["p".to_string()];
        assert_eq!(rerun_failed_command("go test -count 1 ./...", &go), None);
        assert_eq!(rerun_failed_command("go test ./... -args -x", &go), None);
        assert_eq!(rerun_failed_command("go test -run=X ./...", &go), None);
        assert_eq!(rerun_failed_command("go test -run X ./...", &go), None);
        assert_eq!(
            rerun_failed_command("go test -count=1 -v ./...", &go).as_deref(),
            Some("go test -count=1 -v -run '^(TestA)$' 'p'")
        );
    }

    #[test]
    fn rerun_nextest_filters_and_positionals() {
        let nextest = run(TestTool::Nextest, &["a"]);
        assert_eq!(
            rerun_failed_command("cargo nextest run --filter-expr 'x'", &nextest),
            None
        );
        assert_eq!(
            rerun_failed_command("cargo nextest run --filter-expr=test(x)", &nextest),
            None
        );
        assert_eq!(
            rerun_failed_command("cargo nextest run foo", &nextest).as_deref(),
            Some("cargo nextest run foo -E 'test(=a)'")
        );
    }

    #[test]
    fn rerun_package_manager_dashes_only_for_npm() {
        let jest = run(TestTool::Jest, &["a › b"]);
        assert_eq!(
            rerun_failed_command("npm run jest", &jest).as_deref(),
            Some("npm run jest -- -t 'a b'")
        );
        assert_eq!(
            rerun_failed_command("yarn jest", &jest).as_deref(),
            Some("yarn jest -t 'a b'")
        );
        assert_eq!(
            rerun_failed_command("npm test -- --watch", &jest).as_deref(),
            Some("npm test -- --watch -t 'a b'")
        );
        assert_eq!(rerun_failed_command("npm test # x", &jest), None);
        assert_eq!(rerun_failed_command("npm test \\x", &jest), None);
    }

    fn insight(command: &str, text: &str) -> Option<Insight> {
        recognise(command, &fixture(text), Some(0))
    }

    fn build_errors(text: &str) -> Vec<(String, u32, Option<u32>)> {
        match insight("cargo build", text) {
            Some(Insight::Build { errors }) => errors
                .into_iter()
                .map(|error| (error.path.display().to_string(), error.line, error.column))
                .collect(),
            other => panic!("expected Build, got {other:?}"),
        }
    }

    fn status_rows(command: &str, text: &str) -> Vec<(i64, String, FileState)> {
        match insight(command, text) {
            Some(Insight::GitStatus { files }) => files
                .into_iter()
                .map(|file| (file.absolute_line, file.path, file.state))
                .collect(),
            other => panic!("expected GitStatus, got {other:?}"),
        }
    }

    fn table_of(command: &str, text: &str) -> Table {
        match insight(command, text) {
            Some(Insight::Table(table)) => table,
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn rustc_errors_located() {
        let text = include_str!("../test_fixtures/smart/rustc_errors.txt");
        assert_eq!(
            build_errors(text),
            vec![
                ("src/main.rs".to_string(), 4, Some(13)),
                ("src/lib.rs".to_string(), 12, Some(5)),
            ]
        );
    }

    #[test]
    fn tsc_and_gcc_errors_located() {
        let text = "src/app.ts(10,5): error TS2322: Type 'string' is not assignable.\nmain.c:7:3: error: expected ';'\nmain.c:8:1: warning: unused\n";
        assert_eq!(
            build_errors(text),
            vec![
                ("src/app.ts".to_string(), 10, Some(5)),
                ("main.c".to_string(), 7, Some(3)),
            ]
        );
    }

    #[test]
    fn successful_build_has_no_insight() {
        let text = "   Compiling demo v0.1.0\nwarning: unused variable\n --> src/main.rs:1:1\n    Finished dev profile\n";
        assert_eq!(insight("cargo build", text), None);
    }

    #[test]
    fn git_status_long_format() {
        let rows = status_rows(
            "git status",
            include_str!("../test_fixtures/smart/git_status_long.txt"),
        );
        let expected = [
            (3, "a.txt", FileState::Staged),
            (4, "d.txt", FileState::Staged),
            (9, "b.txt", FileState::Unstaged),
            (13, "a b.txt", FileState::Untracked),
            (14, "new.txt", FileState::Untracked),
            (15, "é.txt", FileState::Untracked),
        ];
        assert_eq!(
            rows,
            expected
                .into_iter()
                .map(|(line, path, state)| (line, path.to_string(), state))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn git_status_short_format() {
        let rows = status_rows(
            "git status -sb",
            include_str!("../test_fixtures/smart/git_status_short.txt"),
        );
        let states: Vec<_> = rows.iter().map(|row| (row.1.as_str(), row.2)).collect();
        assert_eq!(
            states,
            vec![
                ("a.txt", FileState::Staged),
                ("b.txt", FileState::Unstaged),
                ("d.txt", FileState::Staged),
                ("a b.txt", FileState::Untracked),
                ("new.txt", FileState::Untracked),
                ("é.txt", FileState::Untracked),
            ]
        );
        assert_eq!(rows[0].0, 1);
    }

    #[test]
    fn git_status_short_format_can_list_a_file_twice() {
        let rows = status_rows("git status -s", "MM both.rs\n");
        assert_eq!(
            rows,
            vec![
                (0, "both.rs".to_string(), FileState::Staged),
                (0, "both.rs".to_string(), FileState::Unstaged),
            ]
        );
    }

    #[test]
    fn git_status_paths_with_spaces_and_renames() {
        let rows = status_rows(
            "git status --short",
            "?? \"a b.txt\"\nR  old.txt -> new.txt\nRM \"x y\" -> \"p q\"\n?? \"\\303\\251.txt\"\n",
        );
        let paths: Vec<_> = rows.iter().map(|row| row.1.as_str()).collect();
        assert_eq!(paths, vec!["a b.txt", "new.txt", "p q", "p q", "é.txt"]);
    }

    #[test]
    fn git_status_needs_the_git_status_command() {
        assert_eq!(insight("echo", "?? new.txt\n"), None);
    }

    #[test]
    fn git_log_hashes_with_columns() {
        let hashes = |command: &str, text: &str| match insight(command, text) {
            Some(Insight::GitLog { commits }) => commits
                .into_iter()
                .map(|commit| (commit.absolute_line, commit.columns, commit.hash))
                .collect::<Vec<_>>(),
            other => panic!("expected GitLog, got {other:?}"),
        };
        assert_eq!(
            hashes(
                "git log",
                include_str!("../test_fixtures/smart/git_log_default.txt")
            ),
            vec![
                (
                    0,
                    7..47,
                    "3c170bdd4d90abd616df40a0a0fa99fef681a312".to_string()
                ),
                (
                    6,
                    7..47,
                    "f05df5ee50a7f6137bdcf656b547dfd65d0a5c5c".to_string()
                ),
            ]
        );
        assert_eq!(
            hashes(
                "git log --oneline",
                include_str!("../test_fixtures/smart/git_log_oneline.txt")
            ),
            vec![
                (0, 0..7, "3c170bd".to_string()),
                (1, 0..7, "f05df5e".to_string())
            ]
        );
    }

    #[test]
    fn json_detected() {
        assert_eq!(
            insight("curl x", "{\n  \"a\": [1, 2],\n  \"b\": null\n}\n"),
            Some(Insight::Json)
        );
        assert_eq!(insight("cat x", "[1, 2, 3]"), Some(Insight::Json));
    }

    #[test]
    fn json_near_misses() {
        for text in ["{not json", "[1,2", "ok {\n}"] {
            assert_eq!(insight("cat x", text), None, "{text:?}");
        }
    }

    #[test]
    fn kubectl_docker_ps_tables() {
        let pods = table_of(
            "kubectl get pods",
            include_str!("../test_fixtures/smart/kubectl_pods.txt"),
        );
        assert_eq!(pods.headers, ["NAME", "READY", "STATUS", "RESTARTS", "AGE"]);
        assert_eq!(
            pods.rows[1],
            [
                "worker-5c6d7f8b9-xyz12",
                "0/1",
                "CrashLoopBackOff",
                "12",
                "2h"
            ]
        );

        let containers = table_of(
            "docker ps",
            include_str!("../test_fixtures/smart/docker_ps.txt"),
        );
        assert_eq!(containers.headers[0], "CONTAINER ID");
        assert_eq!(containers.headers.len(), 7);
        assert_eq!(containers.rows[0][5], "0.0.0.0:5432->5432/tcp");
        assert_eq!(containers.rows[1][6], "cache");

        let processes = table_of("ps aux", include_str!("../test_fixtures/smart/ps_aux.txt"));
        assert_eq!(
            processes.headers.last().map(String::as_str),
            Some("COMMAND")
        );
        assert_eq!(
            processes.rows[1].last().map(String::as_str),
            Some("/bin/zsh -c echo hello world")
        );
    }

    #[test]
    fn prose_is_not_a_table() {
        let text = "The quick brown fox jumps over the lazy dog and keeps running far away.\nA second sentence follows here, which is of a quite different length.\nAnd then a short third one.\n";
        assert_eq!(insight("cat notes", text), None);
    }

    #[test]
    fn two_line_output_is_not_a_table() {
        assert_eq!(insight("ls", "NAME   AGE\nweb    3d\n"), None);
    }

    #[test]
    fn local_urls() {
        assert_eq!(
            find_local_url("  ➜  Local:   http://localhost:5173/"),
            Some("http://localhost:5173/".to_string())
        );
        assert_eq!(
            find_local_url("listening on http://0.0.0.0:3000"),
            Some("http://localhost:3000".to_string())
        );
        assert_eq!(
            find_local_url("at http://[::]:8080/app"),
            Some("http://localhost:8080/app".to_string())
        );
        assert_eq!(
            find_local_url("http://127.0.0.1:9000/x"),
            Some("http://127.0.0.1:9000/x".to_string())
        );
        assert_eq!(find_local_url("https://example.com"), None);
        assert_eq!(url_label("http://localhost:5173/"), "localhost:5173");
        assert_eq!(url_label("http://localhost:3000/a/b"), "localhost:3000");
    }

    #[test]
    fn local_url_needs_a_host_boundary() {
        assert_eq!(find_local_url("http://localhost.example.com/"), None);
        assert_eq!(find_local_url("http://127.0.0.10:80"), None);
        assert_eq!(
            find_local_url("see http://localhost, ok"),
            Some("http://localhost".to_string())
        );
    }

    #[test]
    fn blank_rows_do_not_make_a_table() {
        assert_eq!(insight("ls", "NAME  AGE\n\nweb   3d\n"), None);
        let long_header = format!("{} B\nx y\nz w\n", "A".repeat(1_100));
        assert_eq!(insight("ls", &long_header), None);
    }

    #[test]
    fn git_paths_decode_control_escapes() {
        let rows = status_rows("git status -s", "?? \"a\\tb\\r.txt\"\n");
        assert_eq!(rows[0].1, "a\tb\r.txt");
    }

    #[test]
    fn error_lines() {
        assert!(is_error_line("error: x"));
        assert!(is_error_line("thread 'main' panicked at src/lib.rs"));
        assert!(is_error_line("Test FAILED"));
        assert!(!is_error_line("0 errors"));
        assert!(!is_error_line("Compiled with no errors"));
        assert!(!is_error_line("all good"));
    }

    #[test]
    fn csv_quoting() {
        let table = Table {
            headers: vec!["a".to_string(), "b,c".to_string()],
            rows: vec![vec!["say \"hi\"".to_string(), "line1\nline2".to_string()]],
        };
        assert_eq!(
            table_to_csv(&table),
            "a,\"b,c\"\n\"say \"\"hi\"\"\",\"line1\nline2\"\n"
        );
    }
}
