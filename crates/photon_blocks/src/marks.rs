use percent_encoding::percent_decode;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellMark {
    PromptStart,
    CommandStart,
    OutputStart,
    CommandEnd {
        exit_code: Option<i32>,
    },
    CommandText {
        command: String,
        save_to_history: bool,
    },
    Cwd(PathBuf),
}

/// `params` are the OSC fields after the OSC number, so the first field
/// decides which OSC (133, 633 or 7) the mark came from.
pub fn parse_shell_mark(params: &[Vec<u8>]) -> Option<ShellMark> {
    let first = std::str::from_utf8(params.first()?).ok()?;
    let field = |index: usize| {
        params
            .get(index)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
    };
    match first {
        "A" => Some(ShellMark::PromptStart),
        "B" => Some(ShellMark::CommandStart),
        "C" => Some(ShellMark::OutputStart),
        "D" => Some(ShellMark::CommandEnd {
            exit_code: field(1).and_then(|code| code.parse().ok()),
        }),
        "E" => Some(ShellMark::CommandText {
            command: unescape_command_text(field(1)?),
            save_to_history: field(2) != Some("nohist"),
        }),
        url => {
            let rest = url.strip_prefix("file://")?;
            let path = &rest[rest.find('/')?..];
            let decoded = percent_decode(path.as_bytes()).decode_utf8_lossy();
            Some(ShellMark::Cwd(PathBuf::from(decoded.into_owned())))
        }
    }
}

/// Mirrors the zsh side: `\`, `;`, BEL, ESC and newline become `\xHH`.
pub fn escape_command_text(command: &str) -> String {
    let mut escaped = String::with_capacity(command.len());
    for character in command.chars() {
        match character {
            '\\' | ';' | '\u{7}' | '\u{1b}' | '\n' => {
                escaped.push_str(&format!("\\x{:02x}", character as u32))
            }
            other => escaped.push(other),
        }
    }
    escaped
}

fn unescape_command_text(escaped: &str) -> String {
    let bytes = escaped.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = (bytes[index] == b'\\' && bytes.get(index + 1) == Some(&b'x'))
            .then(|| bytes.get(index + 2..index + 4))
            .flatten()
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 16).ok());
        match hex {
            Some(byte) => {
                decoded.push(byte);
                index += 4;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(fields: &[&str]) -> Option<ShellMark> {
        let params: Vec<Vec<u8>> = fields
            .iter()
            .map(|field| field.as_bytes().to_vec())
            .collect();
        parse_shell_mark(&params)
    }

    #[test]
    fn parses_all_mark_kinds() {
        assert_eq!(parse(&["A"]), Some(ShellMark::PromptStart));
        assert_eq!(parse(&["B"]), Some(ShellMark::CommandStart));
        assert_eq!(parse(&["C"]), Some(ShellMark::OutputStart));
        assert_eq!(
            parse(&["D", "130"]),
            Some(ShellMark::CommandEnd {
                exit_code: Some(130)
            })
        );
        assert_eq!(
            parse(&["D"]),
            Some(ShellMark::CommandEnd { exit_code: None })
        );
        assert_eq!(
            parse(&["E", "echo a\\x3bb"]),
            Some(ShellMark::CommandText {
                command: "echo a;b".into(),
                save_to_history: true
            })
        );
        assert_eq!(
            parse(&["E", "ls", "nohist"]),
            Some(ShellMark::CommandText {
                command: "ls".into(),
                save_to_history: false
            })
        );
        assert_eq!(
            parse(&["file://host/Users/a%20b"]),
            Some(ShellMark::Cwd(PathBuf::from("/Users/a b")))
        );
    }

    #[test]
    fn unknown_marks_are_none() {
        assert_eq!(parse(&["Z"]), None);
        assert_eq!(parse(&["P", "Cwd=/"]), None);
        assert_eq!(parse(&[]), None);
    }

    #[test]
    fn escape_round_trips() {
        let command = "a\\b;c\u{7}d\u{1b}e\nf é";
        let escaped = escape_command_text(command);
        assert!(!escaped.contains(';') && !escaped.contains('\n'));
        assert!(escaped.contains("\\x5c"));
        assert_eq!(
            parse(&["E", &escaped]),
            Some(ShellMark::CommandText {
                command: command.into(),
                save_to_history: true
            })
        );
    }
}
