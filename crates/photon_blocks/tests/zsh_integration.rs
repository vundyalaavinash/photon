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

fn run_zsh(user_zshrc: &str, input: &str) -> (Vec<ShellMark>, String, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("home");
    std::fs::write(home.path().join(".zshrc"), user_zshrc).expect("write zshrc");
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

    let mut env = HashMap::new();
    assert!(prepare_zsh_env("/bin/zsh", &directory, &mut env));
    let mut child = Command::new("zsh")
        .arg("-i")
        .env_clear()
        .env("HOME", home.path())
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
