use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use db::{
    kvp::KeyValueStore,
    sqlez::{domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection},
};
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    Global, IntoElement, Render, SharedString, Task, WeakEntity, Window, actions, rems,
};
use photon_blocks::Block;
use picker::{Picker, PickerDelegate};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt as _, paths::PathExt as _};
use workspace::ModalView;

use crate::BlockInput;

actions!(
    photon_terminal,
    [
        /// Clears the command history of every Photon terminal.
        ClearHistory,
        /// Opens a searchable list of previously run commands.
        SearchHistory,
    ]
);

const MAX_ENTRIES: usize = 10_000;
const PICKER_LIMIT: usize = 100;
const IMPORTED_KEY: &str = "photon_terminal_history_imported";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub exit_code: Option<i32>,
    pub ran_at: Option<SystemTime>,
}

impl HistoryEntry {
    /// None for blocks the shell asked not to save and for blank commands.
    pub fn from_block(block: &Block) -> Option<Self> {
        (block.save_to_history && !block.command.trim().is_empty()).then(|| Self {
            command: block.command.clone(),
            cwd: block.cwd.clone(),
            exit_code: block.exit_code,
            ran_at: Some(SystemTime::now()),
        })
    }

    pub(crate) fn imported(command: String) -> Self {
        Self {
            command,
            cwd: None,
            exit_code: None,
            ran_at: None,
        }
    }
}

/// Plain lines and extended-format lines (`: <timestamp>:<duration>;command`), where a
/// trailing backslash continues the command on the next line. Oldest first.
pub fn parse_zsh_histfile(contents: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut pending: Option<String> = None;
    for line in contents.lines() {
        let line = match pending.take() {
            Some(mut previous) => {
                previous.push('\n');
                previous.push_str(line);
                previous
            }
            None => strip_extended_prefix(line).to_string(),
        };
        match line.strip_suffix('\\') {
            Some(without_backslash) => pending = Some(without_backslash.to_string()),
            None if line.is_empty() => {}
            None => commands.push(line),
        }
    }
    commands.extend(pending.filter(|command| !command.is_empty()));
    commands
}

fn strip_extended_prefix(line: &str) -> &str {
    line.strip_prefix(": ")
        .and_then(|rest| {
            let (metadata, command) = rest.split_once(';')?;
            let (timestamp, duration) = metadata.split_once(':')?;
            let numeric = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
            (numeric(timestamp) && numeric(duration)).then_some(command)
        })
        .unwrap_or(line)
}

pub struct HistoryDb(ThreadSafeConnection);

impl Domain for HistoryDb {
    const NAME: &str = "photon_terminal_history";

    const MIGRATIONS: &[&str] = &["CREATE TABLE IF NOT EXISTS history(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            command TEXT NOT NULL,
            cwd TEXT,
            exit_code INTEGER,
            ran_at INTEGER
        ) STRICT;"];
}

db::static_connection!(HistoryDb, []);

type Row = (String, Option<String>, Option<i32>, Option<i64>);

impl HistoryDb {
    /// Newest first.
    fn load(&self) -> anyhow::Result<Vec<HistoryEntry>> {
        let rows = self.select::<Row>(&format!(
            "SELECT command, cwd, exit_code, ran_at FROM history ORDER BY id DESC LIMIT {MAX_ENTRIES}"
        ))?()?;
        Ok(rows
            .into_iter()
            .map(|(command, cwd, exit_code, ran_at)| HistoryEntry {
                command,
                cwd: cwd.map(PathBuf::from),
                exit_code,
                ran_at: ran_at
                    .and_then(|seconds| u64::try_from(seconds).ok())
                    .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds)),
            })
            .collect())
    }

    /// Oldest first, so ids grow with recency.
    async fn insert(&self, entries: Vec<HistoryEntry>) -> anyhow::Result<()> {
        self.write(move |connection| {
            connection.with_savepoint("insert_history", || {
                let mut insert = Statement::prepare(
                    connection,
                    "INSERT INTO history(command, cwd, exit_code, ran_at) VALUES (?1, ?2, ?3, ?4)",
                )?;
                for entry in &entries {
                    let cwd = entry
                        .cwd
                        .as_ref()
                        .map(|cwd| cwd.to_string_lossy().into_owned());
                    let ran_at = entry
                        .ran_at
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok());
                    let mut index = insert.bind(&entry.command, 1)?;
                    index = insert.bind(&cwd, index)?;
                    index = insert.bind(&entry.exit_code, index)?;
                    insert.bind(&ran_at, index)?;
                    insert.exec()?;
                }
                connection.exec(&format!(
                    "DELETE FROM history WHERE id NOT IN \
                     (SELECT id FROM history ORDER BY id DESC LIMIT {MAX_ENTRIES})"
                ))?()
            })
        })
        .await
    }

    async fn clear(&self) -> anyhow::Result<()> {
        self.write(|connection| connection.exec("DELETE FROM history")?())
            .await
    }
}

struct GlobalHistoryStore(Entity<HistoryStore>);

impl Global for GlobalHistoryStore {}

pub struct HistoryStore {
    /// Newest first.
    entries: VecDeque<HistoryEntry>,
    db: Option<HistoryDb>,
    _load: Task<()>,
}

impl HistoryStore {
    /// Creates the global store, importing `histfile` the first time the history is empty.
    pub fn init_global(histfile: Option<PathBuf>, cx: &mut App) {
        let store = cx.new(|cx| Self::persistent(histfile, cx));
        cx.set_global(GlobalHistoryStore(store));
    }

    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalHistoryStore>() {
            return global.0.clone();
        }
        Self::init_global(None, cx);
        cx.global::<GlobalHistoryStore>().0.clone()
    }

    fn persistent(histfile: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        Self::with_db(
            HistoryDb::global(cx),
            KeyValueStore::global(cx),
            histfile,
            cx,
        )
    }

    fn with_db(
        db: HistoryDb,
        kvp: KeyValueStore,
        histfile: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let load = cx.spawn({
            let db = db.clone();
            async move |this, cx| {
                let loaded = cx
                    .background_spawn(load_and_import(db, kvp, histfile))
                    .await
                    .log_err();
                if let Some(loaded) = loaded {
                    this.update(cx, |this, cx| {
                        this.entries.extend(loaded);
                        this.entries.truncate(MAX_ENTRIES);
                        cx.notify();
                    })
                    .log_err();
                }
            }
        });
        Self {
            entries: VecDeque::new(),
            db: Some(db),
            _load: load,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_global(store: Entity<Self>, cx: &mut App) {
        cx.set_global(GlobalHistoryStore(store));
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Self {
        Self {
            entries: VecDeque::new(),
            db: None,
            _load: Task::ready(()),
        }
    }

    pub fn record(&mut self, entry: HistoryEntry, cx: &mut Context<Self>) {
        if let Some(db) = self.db.clone() {
            let entry = entry.clone();
            cx.background_spawn(async move { db.insert(vec![entry]).await.log_err() })
                .detach();
        }
        self.entries.push_front(entry);
        self.entries.truncate(MAX_ENTRIES);
        cx.notify();
    }

    /// Newest first, keeping the newest occurrence of each command.
    pub fn recent(&self, limit: usize) -> Vec<HistoryEntry> {
        self.distinct(self.entries.iter(), limit)
    }

    /// The grey autosuggestion for `buffer`; see [`crate::history_suggestion`].
    pub fn suggestion(&self, buffer: &str) -> Option<String> {
        let (newer, older) = self.entries.as_slices();
        crate::history_suggestion(buffer, newer)
            .or_else(|| crate::history_suggestion(buffer, older))
    }

    // ponytail: case-insensitive subsequence match in recency order, not scored; swap in
    // `fuzzy::match_strings` if ranking by match quality matters.
    pub fn search(&self, query: &str, limit: usize) -> Vec<HistoryEntry> {
        let query = query.to_lowercase();
        self.distinct(
            self.entries
                .iter()
                .filter(|entry| is_subsequence(&query, &entry.command.to_lowercase())),
            limit,
        )
    }

    pub fn recent_dirs(&self, limit: usize) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        self.entries
            .iter()
            .filter_map(|entry| entry.cwd.as_ref())
            .filter(|cwd| seen.insert(*cwd))
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();
        if let Some(db) = self.db.clone() {
            cx.background_spawn(async move { db.clear().await.log_err() })
                .detach();
        }
        cx.notify();
    }

    fn distinct<'a>(
        &self,
        entries: impl Iterator<Item = &'a HistoryEntry>,
        limit: usize,
    ) -> Vec<HistoryEntry> {
        let mut seen = HashSet::new();
        entries
            .filter(|entry| seen.insert(entry.command.as_str()))
            .take(limit)
            .cloned()
            .collect()
    }
}

pub(crate) fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut haystack = haystack.chars();
    needle
        .chars()
        .all(|wanted| haystack.any(|candidate| candidate == wanted))
}

async fn load_and_import(
    db: HistoryDb,
    kvp: KeyValueStore,
    histfile: Option<PathBuf>,
) -> anyhow::Result<Vec<HistoryEntry>> {
    let entries = db.load()?;
    let Some(histfile) = histfile else {
        return Ok(entries);
    };
    if !entries.is_empty() || kvp.read_kvp(IMPORTED_KEY)?.is_some() {
        return Ok(entries);
    }
    let Some(imported) = read_histfile(&histfile)? else {
        return Ok(entries);
    };
    db.insert(imported.iter().rev().cloned().collect()).await?;
    kvp.write_kvp(IMPORTED_KEY.into(), "1".into()).await?;
    Ok(imported)
}

/// Newest first and capped; None when the file does not exist.
fn read_histfile(path: &Path) -> anyhow::Result<Option<Vec<HistoryEntry>>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // zsh stores some bytes in a metafied form that is not valid UTF-8.
    let contents = String::from_utf8_lossy(&bytes);
    Ok(Some(
        parse_zsh_histfile(&contents)
            .into_iter()
            .rev()
            .take(MAX_ENTRIES)
            .map(HistoryEntry::imported)
            .collect(),
    ))
}

/// `$HISTFILE` from the process environment, else `~/.zsh_history`.
pub fn default_histfile() -> Option<PathBuf> {
    std::env::var_os("HISTFILE")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(util::paths::home_dir().join(".zsh_history")))
}

pub struct HistoryPicker {
    picker: Entity<Picker<HistoryPickerDelegate>>,
}

impl HistoryPicker {
    pub fn new(
        input: WeakEntity<BlockInput>,
        store: Entity<HistoryStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = HistoryPickerDelegate {
            picker: cx.entity().downgrade(),
            input,
            store,
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for HistoryPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(40.)).child(self.picker.clone())
    }
}

impl Focusable for HistoryPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for HistoryPicker {}
impl ModalView for HistoryPicker {}

pub struct HistoryPickerDelegate {
    picker: WeakEntity<HistoryPicker>,
    input: WeakEntity<BlockInput>,
    store: Entity<HistoryStore>,
    matches: Vec<HistoryEntry>,
    selected_index: usize,
}

impl PickerDelegate for HistoryPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "terminal history"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search command history…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.matches = self.store.read(cx).search(&query, PICKER_LIMIT);
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(entry) = self.matches.get(self.selected_index) {
            let command = entry.command.clone();
            self.input
                .update(cx, |input, cx| input.set_text(&command, window, cx))
                .log_err();
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.matches.get(ix)?;
        let failed = entry.exit_code.is_some_and(|code| code != 0);
        let secondary = SharedString::from(
            [
                entry
                    .cwd
                    .as_ref()
                    .map(|cwd| cwd.compact().to_string_lossy().into_owned()),
                failed.then(|| "✗".to_string()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("  "),
        );
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(Label::new(entry.command.clone()).single_line())
                        .when(!secondary.is_empty(), |this| {
                            this.child(
                                Label::new(secondary)
                                    .size(LabelSize::Small)
                                    .color(if failed { Color::Error } else { Color::Muted })
                                    .single_line(),
                            )
                        }),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use photon_blocks::BlockId;

    fn entry(command: &str) -> HistoryEntry {
        HistoryEntry::imported(command.to_string())
    }

    fn block(command: &str, save_to_history: bool) -> Block {
        Block {
            id: BlockId(0),
            command: command.into(),
            save_to_history,
            cwd: Some(PathBuf::from("/tmp")),
            branch: None,
            prompt_line: 0,
            command_line: 0,
            output_line: None,
            end_line: None,
            started_at: None,
            duration: None,
            exit_code: Some(0),
        }
    }

    fn commands(entries: Vec<HistoryEntry>) -> Vec<String> {
        entries.into_iter().map(|entry| entry.command).collect()
    }

    #[test]
    fn parse_zsh_histfile_handles_extended_and_continuations() {
        let contents = "ls\n: 1700000000:0;git status\n: 1700000001:2;for f in a b\\\ndo echo $f\\\ndone\n\necho a;b\n";
        assert_eq!(
            parse_zsh_histfile(contents),
            vec![
                "ls",
                "git status",
                "for f in a b\ndo echo $f\ndone",
                "echo a;b"
            ]
        );
    }

    #[test]
    fn record_skips_nohist() {
        assert!(HistoryEntry::from_block(&block(" secret", false)).is_none());
        assert!(HistoryEntry::from_block(&block("  ", true)).is_none());
        let saved = HistoryEntry::from_block(&block("ls", true)).expect("saved");
        assert_eq!(saved.command, "ls");
        assert_eq!(saved.cwd, Some(PathBuf::from("/tmp")));
        assert!(saved.ran_at.is_some());
    }

    #[gpui::test]
    fn cap_is_10000(cx: &mut TestAppContext) {
        let store = cx.new(|_| HistoryStore::in_memory());
        store.update(cx, |store, cx| {
            for index in 0..=MAX_ENTRIES {
                store.record(entry(&format!("cmd {index}")), cx);
            }
        });
        let recent = store.read_with(cx, |store, _| store.recent(usize::MAX));
        assert_eq!(recent.len(), MAX_ENTRIES);
        assert_eq!(recent[0].command, format!("cmd {MAX_ENTRIES}"));
        assert_eq!(recent[MAX_ENTRIES - 1].command, "cmd 1");
    }

    #[gpui::test]
    fn recent_is_newest_first_and_deduped(cx: &mut TestAppContext) {
        let store = cx.new(|_| HistoryStore::in_memory());
        store.update(cx, |store, cx| {
            for command in ["a", "b", "a", "c"] {
                store.record(entry(command), cx);
            }
        });
        store.read_with(cx, |store, _| {
            assert_eq!(commands(store.recent(10)), ["c", "a", "b"]);
            assert_eq!(commands(store.recent(2)), ["c", "a"]);
            assert_eq!(commands(store.search("a", 10)), ["a"]);
        });
    }

    #[gpui::test]
    fn clear_empties(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let db = cx
            .foreground_executor()
            .block_test(HistoryDb::open_test_db("clear_empties"));
        let kvp = cx.update(|cx| KeyValueStore::global(cx));
        let store = cx.new(|cx| HistoryStore::with_db(db.clone(), kvp.clone(), None, cx));
        store.update(cx, |store, cx| store.record(entry("ls"), cx));
        cx.run_until_parked();
        store.update(cx, |store, cx| store.clear(cx));
        cx.run_until_parked();
        assert!(store.read_with(cx, |store, _| store.recent(10).is_empty()));

        let reloaded = cx.new(|cx| HistoryStore::with_db(db, kvp, None, cx));
        cx.run_until_parked();
        assert!(reloaded.read_with(cx, |store, _| store.recent(10).is_empty()));
    }

    #[gpui::test]
    fn import_runs_once(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("tempdir");
        let histfile = directory.path().join("zsh_history");
        std::fs::write(&histfile, ": 1:0;one\n: 2:0;two\n").expect("write histfile");
        let db = cx
            .foreground_executor()
            .block_test(HistoryDb::open_test_db("import_runs_once"));
        let kvp = cx.update(|cx| KeyValueStore::global(cx));

        let first =
            cx.new(|cx| HistoryStore::with_db(db.clone(), kvp.clone(), Some(histfile.clone()), cx));
        cx.run_until_parked();
        first.read_with(cx, |store, _| {
            assert_eq!(commands(store.recent(10)), ["two", "one"]);
            assert_eq!(store.recent(1)[0].cwd, None);
        });

        first.update(cx, |store, cx| store.clear(cx));
        cx.run_until_parked();
        let second = cx.new(|cx| HistoryStore::with_db(db, kvp, Some(histfile), cx));
        cx.run_until_parked();
        assert!(second.read_with(cx, |store, _| store.recent(10).is_empty()));
    }
}
