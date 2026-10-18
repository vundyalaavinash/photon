use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{App, Task};
use photon_blocks::escape_command_text;
use smol::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use util::command::{Child, Stdio, new_command};

use crate::history::HistoryEntry;

const TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub insert: String,
    pub description: Option<String>,
}

struct Helper {
    stdin: Box<dyn AsyncWrite + Unpin>,
    stdout: BufReader<Box<dyn AsyncRead + Unpin>>,
    // Last, so its stdin closes before it is killed.
    _child: Child,
}

impl Helper {
    async fn request(&mut self, request: &str) -> Result<Vec<Completion>> {
        self.stdin.write_all(request.as_bytes()).await?;
        self.stdin.flush().await?;
        let mut completions = Vec::new();
        let mut seen = HashSet::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            if self.stdout.read_until(b'\n', &mut line).await? == 0 {
                bail!("completion helper exited");
            }
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                return Ok(completions);
            }
            // zsh reports a match once per group it appears in.
            if let Some(completion) = parse_line(line)
                && seen.insert(completion.insert.clone())
            {
                completions.push(completion);
            }
        }
    }
}

/// Asks a hidden `zsh -i` (Photon's ZDOTDIR, `PHOTON_COMPLETER=1`) what Tab would complete.
/// The helper starts on the first request and is killed when this is dropped.
pub struct ZshCompleter {
    program: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    helper: Rc<RefCell<Option<Helper>>>,
    warned: Rc<Cell<bool>>,
}

impl ZshCompleter {
    pub fn new(integration_dir: &Path) -> Self {
        let mut env = HashMap::new();
        photon_blocks::prepare_zsh_env("zsh", integration_dir, &mut env);
        // The helper must never print shell marks.
        env.remove("PHOTON_BLOCKS");
        env.insert("PHOTON_COMPLETER".into(), "1".into());
        Self::from_command("zsh", &["-i"], env)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_command(program: &str, args: &[&str], env: HashMap<String, String>) -> Self {
        Self::from_command(program, args, env)
    }

    fn from_command(program: &str, args: &[&str], env: HashMap<String, String>) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            env,
            helper: Rc::default(),
            warned: Rc::default(),
        }
    }

    /// Completions for the word ending at the end of `buffer`. Never fails: a helper that
    /// errors or takes over a second is dropped (restarted next time) and paths are offered.
    pub fn complete(
        &mut self,
        buffer: &str,
        cwd: &Path,
        cx: &App,
    ) -> Task<Result<Vec<Completion>>> {
        let request = format!(
            "{}\t{}\n",
            escape_command_text(&cwd.to_string_lossy()),
            escape_command_text(buffer)
        );
        let existing = self.helper.borrow_mut().take();
        let spawn = existing.is_none().then(|| self.spawn_helper());
        let slot = self.helper.clone();
        let warned = self.warned.clone();
        let timer = cx.background_executor().timer(TIMEOUT);
        let background = cx.background_executor().clone();
        let buffer = buffer.to_string();
        let cwd = cwd.to_path_buf();
        cx.spawn(async move |_| {
            let ask = async move {
                let mut helper = match (existing, spawn) {
                    (Some(helper), _) => helper,
                    (None, Some(spawned)) => spawned?,
                    (None, None) => bail!("no completion helper"),
                };
                let completions = helper.request(&request).await?;
                Ok((helper, completions))
            };
            let timeout = async move {
                timer.await;
                Err(anyhow!("no answer within {TIMEOUT:?}"))
            };
            match smol::future::or(ask, timeout).await {
                Ok((helper, completions)) => {
                    *slot.borrow_mut() = Some(helper);
                    Ok(completions)
                }
                Err(error) => {
                    if !warned.replace(true) {
                        log::warn!("zsh completion failed, completing paths instead: {error:#}");
                    }
                    Ok(background
                        .spawn(async move { path_completions(&buffer, &cwd) })
                        .await)
                }
            }
        })
    }

    fn spawn_helper(&self) -> Result<Helper> {
        let mut child = new_command(&self.program)
            .args(&self.args)
            .env_remove("PHOTON_BLOCKS")
            .env_remove("PHOTON_COMPLETER_PTY")
            .envs(&self.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", self.program))?;
        let stdin = child.stdin.take().context("helper has no stdin")?;
        let stdout = child.stdout.take().context("helper has no stdout")?;
        Ok(Helper {
            stdin: Box::new(stdin),
            stdout: BufReader::new(Box::new(stdout)),
            _child: child,
        })
    }
}

/// Fallback completion: entries of the directory named by the current word.
pub fn path_completions(buffer: &str, cwd: &Path) -> Vec<Completion> {
    let word = unquote(&buffer[word_start(buffer)..]);
    let (directory, prefix) = match word.rfind('/') {
        Some(slash) => word.split_at(slash + 1),
        None => ("", word.as_str()),
    };
    let search = if directory.is_empty() {
        cwd.to_path_buf()
    } else if let Some(rest) = directory.strip_prefix("~/") {
        util::paths::home_dir().join(rest)
    } else {
        cwd.join(directory)
    };
    let Ok(entries) = std::fs::read_dir(&search) else {
        return Vec::new();
    };
    let mut completions: Vec<Completion> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let visible = prefix.starts_with('.') || !name.starts_with('.');
            (visible && name.starts_with(prefix)).then(|| {
                let slash = if entry.path().is_dir() { "/" } else { "" };
                Completion {
                    insert: format!("{}{}{slash}", directory, shell_escape(&name)),
                    description: None,
                }
            })
        })
        .collect();
    completions.sort_by(|a, b| a.insert.cmp(&b.insert));
    completions
}

/// The rest of the newest single-line history command that starts with `buffer`.
pub fn history_suggestion(buffer: &str, recent: &[HistoryEntry]) -> Option<String> {
    if buffer.is_empty() {
        return None;
    }
    recent
        .iter()
        .filter_map(|entry| entry.command.strip_prefix(buffer))
        .find(|rest| !rest.is_empty() && !rest.contains('\n'))
        .map(str::to_string)
}

/// Byte offset where the shell word ending at the end of `text` starts: words split on
/// whitespace outside quotes and not escaped by a backslash.
pub fn word_start(text: &str) -> usize {
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => {}
            (_, '\\') => escaped = true,
            (None, '\'' | '"') => quote = Some(character),
            (None, character) if character.is_whitespace() => start = index + character.len_utf8(),
            _ => {}
        }
    }
    start
}

fn unquote(word: &str) -> String {
    let mut unquoted = String::with_capacity(word.len());
    let mut quote = None;
    let mut characters = word.chars();
    while let Some(character) = characters.next() {
        match (quote, character) {
            (Some(open), character) if character == open => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None | Some('"'), '\\') => unquoted.extend(characters.next()),
            (_, character) => unquoted.push(character),
        }
    }
    unquoted
}

fn shell_escape(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_whitespace() || "\\'\"`$&|;<>()[]{}*?!#~".contains(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn parse_line(line: &str) -> Option<Completion> {
    let (insert, description) = line.split_once('\t').unwrap_or((line, ""));
    if insert.is_empty() {
        return None;
    }
    let description = strip_ansi(description).trim().to_string();
    Some(Completion {
        insert: insert.to_string(),
        description: (!description.is_empty()).then_some(description),
    })
}

/// Drops escape sequences and other control characters a completion function may print.
fn strip_ansi(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\x1b' => match characters.next() {
                Some('[') => {
                    while characters
                        .next()
                        .is_some_and(|byte| !('@'..='~').contains(&byte))
                    {}
                }
                Some(']') => {
                    while let Some(byte) = characters.next() {
                        if byte == '\x07'
                            || (byte == '\x1b' && characters.next_if_eq(&'\\').is_some())
                        {
                            break;
                        }
                    }
                }
                _ => {}
            },
            character if character.is_control() => {}
            character => plain.push(character),
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    const ZSH_FILES: [(&str, &str); 5] = [
        (".zshenv", "zshenv"),
        (".zprofile", "zprofile"),
        (".zshrc", "zshrc"),
        (".zlogin", "zlogin"),
        ("photon-complete.zsh", "photon-complete.zsh"),
    ];

    fn available(program: &str) -> bool {
        let found = which::which(program).is_ok();
        if !found {
            eprintln!("skipping: {program} is not on PATH");
        }
        found
    }

    /// A completer over the bundled zsh files with an empty home, so the developer's own
    /// config cannot change the results.
    fn test_completer(data: &Path, home: &Path) -> ZshCompleter {
        let assets =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/shell_integration/zsh");
        let contents: Vec<(&str, Vec<u8>)> = ZSH_FILES
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
        let directory = photon_blocks::install_zsh_integration(data, &files).expect("install");
        let mut completer = ZshCompleter::new(&directory);
        completer
            .env
            .insert("HOME".into(), home.to_string_lossy().into_owned());
        completer
            .env
            .insert("PHOTON_USER_ZDOTDIR".into(), String::new());
        completer
    }

    fn inserts(completions: &[Completion]) -> Vec<&str> {
        completions
            .iter()
            .map(|completion| completion.insert.as_str())
            .collect()
    }

    #[gpui::test]
    async fn zsh_completes_git_subcommand(cx: &mut TestAppContext) {
        if !available("zsh") || !available("git") {
            return;
        }
        cx.executor().allow_parking();
        let (data, home) = (
            tempfile::tempdir().expect("data"),
            tempfile::tempdir().expect("home"),
        );
        let mut completer = test_completer(data.path(), home.path());
        let completions = cx
            .update(|cx| completer.complete("git che", home.path(), cx))
            .await
            .expect("completions");
        let checkout = completions
            .iter()
            .find(|completion| completion.insert == "checkout")
            .unwrap_or_else(|| panic!("no checkout in {:?}", inserts(&completions)));
        assert!(checkout.description.is_some());
        assert!(
            completer.helper.borrow().is_some(),
            "helper is kept for the next Tab"
        );
    }

    #[gpui::test]
    async fn zsh_completes_files(cx: &mut TestAppContext) {
        if !available("zsh") {
            return;
        }
        cx.executor().allow_parking();
        let (data, home) = (
            tempfile::tempdir().expect("data"),
            tempfile::tempdir().expect("home"),
        );
        let work = tempfile::tempdir().expect("work");
        std::fs::write(work.path().join("alpha.txt"), "").expect("write");
        let mut completer = test_completer(data.path(), home.path());
        let completions = cx
            .update(|cx| completer.complete("cat al", work.path(), cx))
            .await
            .expect("completions");
        assert_eq!(inserts(&completions), ["alpha.txt"]);
        // Same helper, different cwd and a quoted word.
        std::fs::write(home.path().join("my file.txt"), "").expect("write");
        let completions = cx
            .update(|cx| completer.complete("cat 'my", home.path(), cx))
            .await
            .expect("completions");
        assert_eq!(
            inserts(&completions).len(),
            1,
            "{:?}",
            inserts(&completions)
        );
    }

    #[gpui::test]
    async fn slow_helper_falls_back_to_paths(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let work = tempfile::tempdir().expect("work");
        std::fs::write(work.path().join("alpha.txt"), "").expect("write");
        std::fs::create_dir(work.path().join("alps")).expect("mkdir");
        std::fs::write(work.path().join("beta.txt"), "").expect("write");
        let mut completer = ZshCompleter::with_command("sh", &["-c", "sleep 2"], HashMap::new());
        let completions = cx
            .update(|cx| completer.complete("cat al", work.path(), cx))
            .await
            .expect("completions");
        assert_eq!(inserts(&completions), ["alpha.txt", "alps/"]);
        assert!(
            completer.helper.borrow().is_none(),
            "a timed-out helper is restarted"
        );
        assert!(completer.warned.get());
    }

    #[test]
    fn path_completions_handle_dirs_and_spaces() {
        let work = tempfile::tempdir().expect("work");
        std::fs::create_dir_all(work.path().join("src/my dir")).expect("mkdir");
        std::fs::write(work.path().join("src/.hidden"), "").expect("write");
        assert_eq!(
            inserts(&path_completions("cd src/m", work.path())),
            [r"src/my\ dir/"]
        );
        assert_eq!(
            inserts(&path_completions("ls src/", work.path())),
            [r"src/my\ dir/"]
        );
        assert_eq!(
            inserts(&path_completions("ls src/.h", work.path())),
            ["src/.hidden"]
        );
    }

    #[test]
    fn word_start_follows_shell_words() {
        assert_eq!(word_start("git che"), 4);
        assert_eq!(word_start("git "), 4);
        assert_eq!(word_start("cat 'my fi"), 4);
        assert_eq!(word_start(r"cat my\ fi"), 4);
        assert_eq!(word_start("echo \"a b\" c"), 11);
        assert_eq!(word_start("ls"), 0);
    }

    #[test]
    fn completion_lines_parse_and_strip_ansi() {
        assert_eq!(
            parse_line("checkout\t\x1b[1mswitch\x1b[0m branches"),
            Some(Completion {
                insert: "checkout".into(),
                description: Some("switch branches".into()),
            })
        );
        assert_eq!(
            parse_line("alpha.txt\t"),
            Some(Completion {
                insert: "alpha.txt".into(),
                description: None,
            })
        );
        assert_eq!(parse_line("\t"), None);
    }

    #[test]
    fn history_suggestion_is_newest_prefix_match() {
        let recent: Vec<HistoryEntry> = ["git stash", "ls", "git status", "for x\ndo"]
            .into_iter()
            .map(|command| HistoryEntry::imported(command.to_string()))
            .collect();
        assert_eq!(history_suggestion("git s", &recent), Some("tash".into()));
        assert_eq!(history_suggestion("git stat", &recent), Some("us".into()));
        assert_eq!(history_suggestion("ls", &recent), None);
        assert_eq!(history_suggestion("", &recent), None);
        assert_eq!(history_suggestion("for", &recent), None);
    }
}
