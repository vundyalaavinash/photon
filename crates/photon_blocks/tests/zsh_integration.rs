#![allow(
    clippy::disallowed_methods,
    reason = "tests drive a real zsh synchronously"
)]
use photon_blocks::{ShellMark, install_zsh_integration, parse_shell_mark, prepare_zsh_env};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

const FILES: [(&str, &str); 4] = [
    (".zshenv", "zshenv"),
    (".zprofile", "zprofile"),
    (".zshrc", "zshrc"),
    (".zlogin", "zlogin"),
];

fn zsh_available() -> bool {
    let available = Command::new("zsh")
        .arg("-c")
        .arg("true")
        .status()
        .is_ok_and(|status| status.success());
    if !available {
        eprintln!("skipping: zsh is not on PATH");
    }
    available
}

fn osc_sequences(bytes: &[u8]) -> Vec<Vec<Vec<u8>>> {
    let mut sequences = Vec::new();
    let mut rest = bytes;
    while let Some(start) = rest.windows(2).position(|pair| pair == b"\x1b]") {
        rest = &rest[start + 2..];
        let Some(end) = rest.iter().position(|&byte| byte == 7) else {
            break;
        };
        let body = &rest[..end];
        let mut fields = body.split(|&byte| byte == b';').map(<[u8]>::to_vec);
        let number = fields.next().unwrap_or_default();
        let mut params: Vec<Vec<u8>> = fields.collect();
        if number == b"7" {
            // OSC 7 carries a URL, which the terminal surfaces as a single field.
            params = vec![body[2..].to_vec()];
        }
        sequences.push(params);
        rest = &rest[end + 1..];
    }
    sequences
}

fn photon_env(home: &Path) -> (HashMap<String, String>, tempfile::TempDir) {
    let data = tempfile::tempdir().expect("data");
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/shell_integration/zsh");
    let contents: Vec<(&str, Vec<u8>)> = FILES
        .iter()
        .map(|(installed, source)| {
            (
                *installed,
                std::fs::read(assets.join(source)).expect("asset"),
            )
        })
        .collect();
    let files: Vec<(&str, &[u8])> = contents
        .iter()
        .map(|(name, bytes)| (*name, bytes.as_slice()))
        .collect();
    let directory = install_zsh_integration(data.path(), &files).expect("install");
    let mut env = HashMap::from([("HOME".to_string(), home.to_string_lossy().into_owned())]);
    assert!(prepare_zsh_env("/bin/zsh", &directory, &mut env));
    (env, data)
}

fn run_zsh(user_zshrc: &str, input: &str) -> (Vec<ShellMark>, String, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("home");
    std::fs::write(home.path().join(".zshrc"), user_zshrc).expect("write zshrc");
    let (env, _data) = photon_env(home.path());
    let mut child = Command::new("zsh")
        .arg("-i")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .envs(&env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zsh");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("zsh output");
    let mut combined = output.stdout;
    combined.extend(output.stderr);
    let marks = osc_sequences(&combined)
        .iter()
        .filter_map(|params| parse_shell_mark(params))
        .collect();
    (marks, String::from_utf8_lossy(&combined).into_owned(), home)
}

fn command_texts(marks: &[ShellMark]) -> Vec<(String, bool)> {
    marks
        .iter()
        .filter_map(|mark| match mark {
            ShellMark::CommandText {
                command,
                save_to_history,
            } => Some((command.clone(), *save_to_history)),
            _ => None,
        })
        .collect()
}

#[test]
fn zsh_emits_marks_for_each_command() {
    if !zsh_available() {
        return;
    }
    let (marks, _, _home) = run_zsh("", "echo hi\nfalse\ncd /tmp\nexit\n");
    let commands: Vec<String> = command_texts(&marks)
        .into_iter()
        .map(|(command, _)| command)
        .collect();
    assert_eq!(commands[..3], ["echo hi", "false", "cd /tmp"]);
    let codes: Vec<Option<i32>> = marks
        .iter()
        .filter_map(|mark| match mark {
            ShellMark::CommandEnd { exit_code } => Some(*exit_code),
            _ => None,
        })
        .take(3)
        .collect();
    assert_eq!(codes, [Some(0), Some(1), Some(0)]);
    assert!(
        marks
            .iter()
            .any(|mark| matches!(mark, ShellMark::Cwd(path) if path.ends_with("tmp")))
    );
    assert!(marks.contains(&ShellMark::PromptStart));
    assert!(marks.contains(&ShellMark::CommandStart));
    assert!(marks.contains(&ShellMark::OutputStart));
}

#[test]
fn osc7_is_the_first_mark() {
    if !zsh_available() {
        return;
    }
    let (marks, _, _home) = run_zsh("sleep 0.1", "exit\n");
    assert!(
        matches!(marks.first(), Some(ShellMark::Cwd(_))),
        "{marks:?}"
    );
}

#[test]
fn user_rc_still_loads() {
    if !zsh_available() {
        return;
    }
    let rc = "alias hi='echo hi-from-alias'\n";
    let (_, output, home) = run_zsh(rc, "hi\nexit\n");
    assert!(output.contains("hi-from-alias"), "{output}");
    assert_eq!(
        std::fs::read_to_string(home.path().join(".zshrc")).expect("read"),
        rc
    );
}

#[test]
fn command_text_round_trips_special_characters() {
    if !zsh_available() {
        return;
    }
    let command = r#"echo "a;b" 'ü' \\"#;
    let (marks, _, _home) = run_zsh("", &format!("{command}\nexit\n"));
    assert_eq!(command_texts(&marks)[0], (command.to_string(), true));
}

#[test]
fn leading_space_is_nohist() {
    if !zsh_available() {
        return;
    }
    let (marks, _, _home) = run_zsh("", " echo secret\nexit\n");
    assert_eq!(
        command_texts(&marks)[0],
        (" echo secret".to_string(), false)
    );
}

#[test]
fn non_zsh_is_untouched() {
    let mut env = HashMap::from([("A".to_string(), "b".to_string())]);
    let before = env.clone();
    assert!(!prepare_zsh_env("/bin/bash", Path::new("/x"), &mut env));
    assert_eq!(env, before);
}

#[test]
fn non_interactive_zsh_does_not_leak_integration() {
    if !zsh_available() {
        return;
    }
    let home = tempfile::tempdir().expect("home");
    let (env, _data) = photon_env(home.path());
    let output = Command::new("zsh")
        .args(["-c", "echo ${ZDOTDIR-unset}; echo ${PHOTON_BLOCKS-unset}"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .envs(&env)
        .output()
        .expect("zsh -c");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.get(1), Some(&"unset"), "{stdout}");
    assert_eq!(lines.first().map(Path::new), Some(home.path()), "{stdout}");
}

/// Wall time of an interactive zsh running `commands` × `true`, and how many
/// command-end marks it printed.
fn time_true_commands(
    env: &HashMap<String, String>,
    commands: usize,
) -> (std::time::Duration, usize) {
    let mut child = Command::new("zsh")
        .arg("-i")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn zsh");
    let started = std::time::Instant::now();
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{}exit\n", "true\n".repeat(commands)).as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("zsh output");
    let elapsed = started.elapsed();
    let ends = osc_sequences(&output.stdout)
        .iter()
        .filter(|params| matches!(parse_shell_mark(params), Some(ShellMark::CommandEnd { .. })))
        .count();
    (elapsed, ends)
}

#[test]
fn zsh_integration_overhead_under_1ms_per_command() {
    if !zsh_available() {
        return;
    }
    const COMMANDS: usize = 200;
    const RUNS: usize = 5;
    let home = tempfile::tempdir().expect("home");
    std::fs::write(home.path().join(".zshrc"), "").expect("write zshrc");
    let (with_env, _data) = photon_env(home.path());
    // Same startup files; without PHOTON_BLOCKS they install no hooks and print no marks.
    let mut without_env = with_env.clone();
    without_env.remove("PHOTON_BLOCKS");

    let mut with = Vec::new();
    let mut without = Vec::new();
    for _ in 0..RUNS {
        let (elapsed, ends) = time_true_commands(&with_env, COMMANDS);
        assert!(ends >= COMMANDS, "{ends} command-end marks");
        with.push(elapsed);
        let (elapsed, ends) = time_true_commands(&without_env, COMMANDS);
        assert_eq!(ends, 0);
        without.push(elapsed);
    }
    with.sort();
    without.sort();
    let (with, without) = (with[RUNS / 2], without[RUNS / 2]);
    let per_command = with.saturating_sub(without) / COMMANDS as u32;
    eprintln!(
        "{COMMANDS} commands: blocks on {with:?}, off {without:?}, {per_command:?} per command"
    );
    assert!(
        per_command < std::time::Duration::from_millis(1),
        "{per_command:?} per command"
    );
}

/// Replays the marks into a block list, as the terminal would. The B mark lives in the
/// prompt, which zsh prints to stderr, so here it is put back right after each A.
fn block_list(marks: Vec<ShellMark>) -> photon_blocks::BlockList {
    let mut list = photon_blocks::BlockList::new(100);
    let now = std::time::Instant::now();
    for (line, mark) in marks.into_iter().enumerate() {
        let line = line as i64;
        match mark {
            ShellMark::CommandStart => {}
            ShellMark::PromptStart => {
                list.apply(ShellMark::PromptStart, line, now);
                list.apply(ShellMark::CommandStart, line, now);
            }
            mark => {
                list.apply(mark, line, now);
            }
        }
    }
    list
}

fn finished_commands(list: &photon_blocks::BlockList) -> Vec<(String, Option<i32>)> {
    list.blocks()
        .filter(|block| block.end_line.is_some())
        .map(|block| (block.command.clone(), block.exit_code))
        .collect()
}

#[test]
fn preexec_that_sends_output_start_first_keeps_command_text() {
    if !zsh_available() {
        return;
    }
    // What iTerm2's shell integration and p10k's term-shell-integration install.
    let rc = "__other_preexec() { printf '\\e]133;C\\a' }\npreexec_functions+=(__other_preexec)\n";
    let (marks, output, _home) = run_zsh(rc, "echo hi\nexit\n");
    let text = marks
        .iter()
        .position(|mark| matches!(mark, ShellMark::CommandText { .. }));
    let output_start = marks
        .iter()
        .position(|mark| *mark == ShellMark::OutputStart);
    assert!(text < output_start, "{marks:?}");
    let list = block_list(marks);
    assert!(
        !matches!(list.phase(), photon_blocks::Phase::Degraded(_)),
        "{output}"
    );
    assert_eq!(
        finished_commands(&list)[0],
        ("echo hi".to_string(), Some(0))
    );
}

#[test]
fn exec_zsh_keeps_blocks_going() {
    if !zsh_available() {
        return;
    }
    // `-i` because stdin is a pipe here; in a terminal plain `exec zsh` is interactive.
    let (marks, output, _home) = run_zsh("", "exec zsh -i\necho after\nexit\n");
    let list = block_list(marks);
    assert!(
        !matches!(list.phase(), photon_blocks::Phase::Degraded(_)),
        "{output}"
    );
    assert_eq!(
        finished_commands(&list),
        [
            ("exec zsh -i".to_string(), Some(0)),
            ("echo after".to_string(), Some(0))
        ],
        "{output}"
    );
}

#[test]
fn commands_other_than_zsh_see_the_users_zdotdir() {
    if !zsh_available() {
        return;
    }
    // An installer (`conda init zsh`) writes to $ZDOTDIR/.zshrc; Photon's copy would be overwritten.
    let (_marks, output, home) = run_zsh("", "printenv ZDOTDIR\nexit\n");
    let home = home.path().to_string_lossy();
    assert!(
        output.lines().any(|line| line.ends_with(home.as_ref())),
        "{output}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn zsh_writing_to_a_pipe_prints_no_marks() {
    if !zsh_available() {
        return;
    }
    let home = tempfile::tempdir().expect("home");
    let (env, _data) = photon_env(home.path());
    // `script` gives the outer zsh a real terminal; the inner zsh, handed Photon's ZDOTDIR as
    // preexec does for `zsh` commands, writes to a pipe.
    let output = Command::new("script")
        .args(["-q", "/dev/null", "zsh", "-ic"])
        .arg("echo escapes=$(ZDOTDIR=$PHOTON_ZDOTDIR zsh -ic 'echo in''ner' | od -c | tr -d ' ' | grep -c 033)")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .envs(&env)
        .stdin(Stdio::null())
        .output()
        .expect("script");
    let output = String::from_utf8_lossy(&output.stdout);
    // `grep -c` counts the lines of the inner zsh's output that contained an escape.
    assert!(output.contains("escapes=0\r"), "{output}");
}

#[test]
fn blocks_get_padding_rows() {
    if !zsh_available() {
        return;
    }
    let (_marks, output, _home) = run_zsh("", "echo hi\nexit\n");
    // A blank row closes the block's output, and the prompt leaves a blank row and a
    // metadata row above the typed command.
    assert!(
        output.contains("hi\n\n\x1b]133;D;0;photon\x07"),
        "{output:?}"
    );
    assert!(output.contains("\n\n\x1b]133;B;photon\x07"), "{output:?}");
}
