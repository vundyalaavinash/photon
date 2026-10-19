use std::{path::PathBuf, time::Duration};

use anyhow::{Context as _, Result};

use collections::HashMap;
use git_ui::{commit_view::CommitView, project_diff::ProjectDiff};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, PromptLevel, Subscription,
    SystemNotification, SystemNotificationResponse, Task, WeakEntity, Window,
};
use photon_blocks::{
    Block, BlockEvent, BlockId, Insight, MAX_INSIGHT_BYTES, MAX_INSIGHT_LINES, OutputLine,
    RunDelta, StatusFile, compare_runs, find_local_url, format_duration, is_error_line, recognise,
};
use settings::Settings as _;
use terminal::{Event, Terminal, terminal_settings::TerminalSettings};
use util::{ResultExt as _, paths::normalize_lexically};
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::chips::run_git;

/// How many output lines of a running block are searched for a local URL.
const URL_SCAN_LINE_LIMIT: usize = 2_000;

/// A burst larger than this is only checked at its end: notifications are throttled anyway, and
/// copying every line of a large burst out of the grid would stall the UI thread.
const ERROR_SCAN_TAIL_LINES: usize = 200;
/// At most one dev-server error notification per block in this time.
const ERROR_NOTIFICATION_INTERVAL: Duration = Duration::from_secs(60);
const NOTIFICATION_TAG_PREFIX: &str = "photon-block-";
const NOTIFIED_COMMAND_CHARS: usize = 60;
const NOTIFIED_ERROR_CHARS: usize = 120;

pub enum SmartBlocksEvent {
    Changed,
    /// The user activated a notification about this block.
    FocusBlock(BlockId),
}

/// The platform keeps a single notification response handler, so every `SmartBlocks` shares one
/// that hands the response to the entity whose terminal posted the notification.
#[derive(Default)]
struct NotificationRouter {
    listeners: Vec<WeakEntity<SmartBlocks>>,
}

impl Global for NotificationRouter {}

fn route_notification_response(response: SystemNotificationResponse, cx: &mut App) {
    if !TerminalSettings::get_global(cx).smart_blocks {
        return;
    }
    let Some((terminal_id, block_id)) = response
        .tag
        .strip_prefix(NOTIFICATION_TAG_PREFIX)
        .and_then(|rest| rest.split_once('-'))
        .and_then(|(terminal, block)| Some((terminal.parse::<u64>().ok()?, block.parse().ok()?)))
    else {
        return;
    };
    let listeners = cx.global::<NotificationRouter>().listeners.clone();
    for listener in listeners {
        listener
            .update(cx, |smart_blocks, cx| {
                if smart_blocks.terminal.entity_id().as_u64() == terminal_id {
                    cx.emit(SmartBlocksEvent::FocusBlock(BlockId(block_id)));
                }
            })
            .ok();
    }
}

#[derive(Default)]
struct UrlScan {
    /// The block's output start when scanning began; if it moves (lines were trimmed or
    /// renumbered) the position below no longer means anything.
    output_line: i64,
    /// Output lines read so far, which counts against `URL_SCAN_LINE_LIMIT`.
    lines: usize,
    /// The next absolute line to read from.
    next_line: Option<i64>,
}

/// Where a `git status` block's paths are relative to.
struct StatusPaths {
    cwd: PathBuf,
    /// `--porcelain` prints paths from the repository root, other formats from the cwd.
    from_root: bool,
}

impl StatusPaths {
    async fn directory(&self) -> Result<PathBuf> {
        if self.from_root {
            Ok(run_git(&self.cwd, &["rev-parse", "--show-toplevel"], false)
                .await?
                .into())
        } else {
            Ok(self.cwd.clone())
        }
    }
}

/// What a git action did to a `git status` row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowMark {
    Staged,
    Discarded,
}

/// An insight with where the grid had the block when it was read: its line numbers mean
/// nothing once a width change re-wraps the grid.
struct Recognised {
    insight: Insight,
    output_line: i64,
    columns: usize,
}

/// What finished and running command blocks turned out to be, computed off the UI thread.
pub struct SmartBlocks {
    terminal: Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    insights: HashMap<BlockId, Recognised>,
    /// The grid's width when insights were last checked against it.
    columns: usize,
    row_marks: HashMap<(BlockId, String), RowMark>,
    deltas: HashMap<BlockId, RunDelta>,
    urls: HashMap<BlockId, String>,
    scans: HashMap<BlockId, UrlScan>,
    tasks: HashMap<BlockId, Task<()>>,
    window_active: bool,
    error_scans: HashMap<BlockId, UrlScan>,
    /// A block has an entry from its error notification until the interval has passed.
    error_cooldowns: HashMap<BlockId, Task<()>>,
    /// The block count and oldest block when blocks were last checked for removals.
    seen_blocks: (usize, Option<BlockId>),
    _terminal_subscription: Subscription,
    _activation_subscription: Subscription,
}

impl EventEmitter<SmartBlocksEvent> for SmartBlocks {}

impl SmartBlocks {
    pub fn new(
        terminal: Entity<Terminal>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let _terminal_subscription = cx.subscribe(&terminal, Self::handle_terminal_event);
        let _activation_subscription = cx.observe_window_activation(window, |this, window, _| {
            this.window_active = window.is_window_active();
        });
        if !cx.has_global::<NotificationRouter>() {
            cx.set_global(NotificationRouter::default());
            cx.on_system_notification_response(route_notification_response);
        }
        let this = cx.weak_entity();
        let router = cx.global_mut::<NotificationRouter>();
        router
            .listeners
            .retain(|listener| listener.upgrade().is_some());
        router.listeners.push(this);
        let columns = terminal.read(cx).grid_columns();
        Self {
            terminal,
            workspace,
            insights: HashMap::default(),
            columns,
            row_marks: HashMap::default(),
            deltas: HashMap::default(),
            urls: HashMap::default(),
            scans: HashMap::default(),
            tasks: HashMap::default(),
            window_active: window.is_window_active(),
            error_scans: HashMap::default(),
            error_cooldowns: HashMap::default(),
            seen_blocks: (0, None),
            _terminal_subscription,
            _activation_subscription,
        }
    }

    pub fn insight(&self, id: BlockId) -> Option<&Insight> {
        self.insights.get(&id).map(|recognised| &recognised.insight)
    }

    /// The insight only while its line numbers still match the grid; after a width change it is
    /// hidden until the block is recognised again.
    pub fn located_insight(&self, block: &Block, grid_columns: usize) -> Option<&Insight> {
        self.insights
            .get(&block.id)
            .filter(|recognised| is_located(recognised, block, grid_columns))
            .map(|recognised| &recognised.insight)
    }

    pub fn delta(&self, id: BlockId) -> Option<&RunDelta> {
        self.deltas.get(&id)
    }

    pub fn local_url(&self, id: BlockId) -> Option<&str> {
        self.urls.get(&id).map(String::as_str)
    }

    pub fn row_mark(&self, id: BlockId, path: &str) -> Option<RowMark> {
        self.row_marks.get(&(id, path.to_string())).copied()
    }

    pub fn stage(&mut self, id: BlockId, absolute_line: i64, cx: &mut Context<Self>) {
        let Some(file) = self.status_file(id, absolute_line, cx) else {
            return;
        };
        let path = file.path.clone();
        self.run_git_action(
            id,
            vec![path.clone()],
            ["add", "--", &path],
            RowMark::Staged,
            cx,
        );
    }

    pub fn stage_all(&mut self, id: BlockId, cx: &mut Context<Self>) {
        let Some(Insight::GitStatus { files }) = self.insight(id) else {
            return;
        };
        let paths = files.iter().map(|file| file.path.clone()).collect();
        self.run_git_action(id, paths, ["add", "-A"], RowMark::Staged, cx);
    }

    /// Asks first: `git restore` throws the changes away for good.
    pub fn discard(
        &mut self,
        id: BlockId,
        absolute_line: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.status_file(id, absolute_line, cx) else {
            return;
        };
        let path = file.path.clone();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Discard changes to {path}?"),
            None,
            &["Discard", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| {
                this.run_git_action(
                    id,
                    vec![path.clone()],
                    ["restore", "--", &path],
                    RowMark::Discarded,
                    cx,
                )
            })
            .log_err();
        })
        .detach();
    }

    pub fn open_diff(&self, id: BlockId, absolute_line: i64, window: &mut Window, cx: &mut App) {
        let (Some(block_paths), Some(file)) = (
            self.block_paths(id, cx),
            self.status_file(id, absolute_line, cx),
        ) else {
            return;
        };
        let file = file.path.clone();
        let workspace = self.workspace.clone();
        window
            .spawn(cx, async move |cx| {
                let result = async {
                    let path = block_paths.directory().await?.join(&file);
                    Ok::<_, anyhow::Error>(normalize_lexically(&path).unwrap_or(path))
                }
                .await;
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        let project_path = result.and_then(|path| {
                            workspace
                                .project()
                                .read(cx)
                                .find_project_path(&path, cx)
                                .with_context(|| {
                                    format!("{} is not in this project", path.display())
                                })
                        });
                        match project_path {
                            Ok(project_path) => ProjectDiff::deploy_at_project_path(
                                workspace,
                                project_path,
                                window,
                                cx,
                            ),
                            Err(error) => workspace.show_toast(
                                Toast::new(NotificationId::unique::<Self>(), error.to_string()),
                                cx,
                            ),
                        }
                    })
                    .log_err();
            })
            .detach();
    }

    /// Opens `hash` in the innermost repository of the project that contains the block's cwd.
    pub fn open_commit(&self, id: BlockId, hash: &str, window: &mut Window, cx: &mut App) {
        let Some(cwd) = self.block_cwd(id, cx) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let repository = workspace
            .read(cx)
            .project()
            .read(cx)
            .git_store()
            .read(cx)
            .repositories()
            .values()
            .filter(|repository| cwd.starts_with(&repository.read(cx).work_directory_abs_path))
            .max_by_key(|repository| {
                repository
                    .read(cx)
                    .work_directory_abs_path
                    .components()
                    .count()
            })
            .cloned();
        let Some(repository) = repository else {
            self.show_error(
                format!("{} is not in a project repository", cwd.display()),
                cx,
            );
            return;
        };
        CommitView::open(
            hash.to_string(),
            repository.downgrade(),
            self.workspace.clone(),
            None,
            None,
            window,
            cx,
        );
    }

    fn status_file(&self, id: BlockId, absolute_line: i64, cx: &App) -> Option<&StatusFile> {
        let terminal = self.terminal.read(cx);
        let block = terminal.blocks()?.block(id)?;
        let Some(Insight::GitStatus { files }) =
            self.located_insight(block, terminal.grid_columns())
        else {
            return None;
        };
        files
            .iter()
            .find(|file| file.absolute_line == absolute_line)
    }

    fn block_cwd(&self, id: BlockId, cx: &App) -> Option<PathBuf> {
        self.terminal.read(cx).blocks()?.block(id)?.cwd.clone()
    }

    fn block_paths(&self, id: BlockId, cx: &App) -> Option<StatusPaths> {
        let block = self.terminal.read(cx).blocks()?.block(id)?;
        Some(StatusPaths {
            cwd: block.cwd.clone()?,
            from_root: block
                .command
                .split_whitespace()
                .any(|word| word.starts_with("--porcelain")),
        })
    }

    fn run_git_action<const N: usize>(
        &mut self,
        id: BlockId,
        paths: Vec<String>,
        args: [&str; N],
        mark: RowMark,
        cx: &mut Context<Self>,
    ) {
        let Some(block_paths) = self.block_paths(id, cx) else {
            self.show_error("This block has no working directory".to_string(), cx);
            return;
        };
        let args = args.map(str::to_string);
        cx.spawn(async move |this, cx| {
            let args = args.each_ref().map(String::as_str);
            let result = match block_paths.directory().await {
                Ok(directory) => run_git(&directory, &args, true).await,
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    for path in paths {
                        this.row_marks.insert((id, path), mark);
                    }
                    cx.emit(SmartBlocksEvent::Changed);
                }
                Err(error) => this.show_error(error.to_string(), cx),
            })
            .log_err();
        })
        .detach();
    }

    fn show_error(&self, message: String, cx: &mut App) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(Toast::new(NotificationId::unique::<Self>(), message), cx)
            })
            .log_err();
    }

    fn handle_terminal_event(
        &mut self,
        _: Entity<Terminal>,
        event: &Event,
        cx: &mut Context<Self>,
    ) {
        let enabled = TerminalSettings::get_global(cx).smart_blocks;
        match event {
            Event::BlocksChanged(change) => {
                self.forget_removed_blocks(cx);
                if let (true, Some(BlockEvent::Finished(id))) = (enabled, change) {
                    self.notify_finished(*id, cx);
                    self.recognise_block(*id, cx);
                }
                if enabled {
                    self.recognise_moved_blocks(cx);
                }
            }
            Event::Wakeup if enabled => {
                self.scan_running_block_for_url(cx);
                self.scan_running_server_for_errors(cx);
            }
            _ => {}
        }
    }

    fn forget_removed_blocks(&mut self, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        let Some(blocks) = terminal.blocks() else {
            return;
        };
        // Every mark lands here and most remove nothing. Ids only grow, so unless the list shrank
        // the oldest block alone says what was trimmed; `block` would be a linear search each.
        let count = blocks.blocks().size_hint().0;
        let oldest = blocks.blocks().next().map(|block| block.id);
        let (previous_count, previous_oldest) =
            std::mem::replace(&mut self.seen_blocks, (count, oldest));
        if oldest == previous_oldest && count >= previous_count {
            return;
        }
        let live: Option<std::collections::HashSet<BlockId>> =
            (count < previous_count).then(|| blocks.blocks().map(|block| block.id).collect());
        let kept = |id: &BlockId| match &live {
            Some(live) => live.contains(id),
            None => oldest.is_some_and(|oldest| id.0 >= oldest.0),
        };
        let before = self.insights.len() + self.urls.len();
        self.insights.retain(|id, _| kept(id));
        self.deltas.retain(|id, _| kept(id));
        self.urls.retain(|id, _| kept(id));
        self.scans.retain(|id, _| kept(id));
        self.error_scans.retain(|id, _| kept(id));
        self.error_cooldowns.retain(|id, _| kept(id));
        self.tasks.retain(|id, _| kept(id));
        self.row_marks.retain(|(id, _), _| kept(id));
        if self.insights.len() + self.urls.len() != before {
            cx.emit(SmartBlocksEvent::Changed);
        }
    }

    /// A width change re-wraps the grid and moves blocks' lines, so insights that point at
    /// lines are read again.
    fn recognise_moved_blocks(&mut self, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        let columns = terminal.grid_columns();
        if columns == self.columns {
            return;
        }
        self.columns = columns;
        let Some(blocks) = terminal.blocks() else {
            return;
        };
        let moved: Vec<BlockId> = self
            .insights
            .iter()
            .filter(|(id, recognised)| {
                has_line_numbers(&recognised.insight)
                    && !blocks
                        .block(**id)
                        .is_some_and(|block| is_located(recognised, block, columns))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in moved {
            self.recognise_block(id, cx);
        }
    }

    fn recognise_block(&mut self, id: BlockId, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        let Some(block) = terminal.blocks().and_then(|blocks| blocks.block(id)) else {
            return;
        };
        let Some(output_line) = block.output_line else {
            return;
        };
        let columns = terminal.grid_columns();
        let command = block.command.clone();
        let exit_code = block.exit_code;
        let Some(read_lines) =
            terminal.block_output_reader(id, MAX_INSIGHT_LINES, MAX_INSIGHT_BYTES)
        else {
            return;
        };
        let task = cx.background_spawn(async move {
            let (lines, read_columns) = read_lines()?;
            Some((recognise(&command, &lines, exit_code), read_columns))
        });
        let task = cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.tasks.remove(&id);
                match result {
                    // The grid re-wrapped between capturing `output_line` and reading the rows,
                    // so the rows may belong to other lines: read again at the current width.
                    Some((_, read_columns)) if read_columns != columns => {
                        this.recognise_block(id, cx)
                    }
                    Some((insight, _)) => this.store_insight(id, insight, output_line, columns, cx),
                    None => {}
                }
            })
            .log_err();
        });
        self.tasks.insert(id, task);
    }

    fn store_insight(
        &mut self,
        id: BlockId,
        insight: Option<Insight>,
        output_line: i64,
        columns: usize,
        cx: &mut Context<Self>,
    ) {
        let terminal = self.terminal.read(cx);
        let Some(block) = terminal.blocks().and_then(|blocks| blocks.block(id)) else {
            return;
        };
        let Some(insight) = insight else {
            return;
        };
        let recognised = Recognised {
            insight,
            output_line,
            columns,
        };
        // The width changed while the output was read, so it was read from the wrong lines.
        let moved = has_line_numbers(&recognised.insight)
            && !is_located(&recognised, block, terminal.grid_columns());
        if let Insight::Tests(run) = &recognised.insight
            && let Some(delta) = self
                .previous_run(id, cx)
                .map(|previous| compare_runs(previous, run))
        {
            self.deltas.insert(id, delta);
        }
        self.insights.insert(id, recognised);
        if moved {
            self.recognise_block(id, cx);
        }
        cx.emit(SmartBlocksEvent::Changed);
    }

    /// The newest earlier finished block with the same command and directory that was a test run.
    fn previous_run(&self, id: BlockId, cx: &Context<Self>) -> Option<&photon_blocks::TestRun> {
        let blocks = self.terminal.read(cx).blocks()?;
        let current = blocks.block(id)?;
        blocks
            .blocks()
            .rev()
            .skip_while(|block| block.id != id)
            .skip(1)
            .filter(|block| block.command == current.command && block.cwd == current.cwd)
            .find_map(|block| match self.insight(block.id) {
                Some(Insight::Tests(run)) => Some(run),
                _ => None,
            })
    }

    fn scan_running_block_for_url(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .terminal
            .read(cx)
            .blocks()
            .and_then(|blocks| blocks.running())
        else {
            return;
        };
        if self.urls.contains_key(&id) {
            return;
        }
        let scan = self.scans.entry(id).or_default();
        let Some(lines) = read_new_lines(self.terminal.read(cx), id, scan, None) else {
            return;
        };
        if let Some((position, url)) = lines
            .iter()
            .enumerate()
            .find_map(|(position, line)| Some((position, find_local_url(&line.text)?)))
        {
            // The error scan starts at the server's first output, so what came before is never
            // reported, and it is not run (or read) for commands that are not servers.
            self.error_scans.insert(
                id,
                UrlScan {
                    output_line: scan.output_line,
                    lines: 0,
                    next_line: Some(lines[position].absolute_line + 1),
                },
            );
            self.urls.insert(id, url);
            cx.emit(SmartBlocksEvent::Changed);
        }
    }

    fn scan_running_server_for_errors(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .terminal
            .read(cx)
            .blocks()
            .and_then(|blocks| blocks.running())
        else {
            return;
        };
        if !self.urls.contains_key(&id) {
            return;
        }
        let scan = self.error_scans.entry(id).or_default();
        // The budget bounds one read, not the block's life: a server can run for days.
        scan.lines = 0;
        let Some(lines) = read_new_lines(
            self.terminal.read(cx),
            id,
            scan,
            Some(ERROR_SCAN_TAIL_LINES),
        ) else {
            return;
        };
        // Output the user could see, or that came before the server was up, is skipped so
        // it never turns into a stale notification.
        if self.window_active || !self.urls.contains_key(&id) {
            return;
        }
        if self.error_cooldowns.contains_key(&id) {
            return;
        }
        let Some(error_line) = lines.iter().find(|line| is_error_line(&line.text)) else {
            return;
        };
        let Some(command) = self
            .terminal
            .read(cx)
            .blocks()
            .and_then(|blocks| blocks.block(id))
            .map(|block| block.command.clone())
        else {
            return;
        };
        cx.show_system_notification(SystemNotification {
            tag: self.notification_tag(id),
            title: format!("Error in {}", shortened_command(&command)).into(),
            body: error_line
                .text
                .trim()
                .chars()
                .take(NOTIFIED_ERROR_CHARS)
                .collect::<String>()
                .into(),
            actions: Vec::new(),
        });
        let cooldown = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(ERROR_NOTIFICATION_INTERVAL)
                .await;
            this.update(cx, |this, _| this.error_cooldowns.remove(&id))
                .log_err();
        });
        self.error_cooldowns.insert(id, cooldown);
    }

    fn notify_finished(&mut self, id: BlockId, cx: &mut Context<Self>) {
        let threshold = TerminalSettings::get_global(cx).notify_after_seconds;
        if threshold == 0 || self.window_active {
            return;
        }
        let Some(block) = self
            .terminal
            .read(cx)
            .blocks()
            .and_then(|blocks| blocks.block(id))
        else {
            return;
        };
        let (Some(duration), Some(exit_code)) = (block.duration, block.exit_code) else {
            return;
        };
        if duration < Duration::from_secs(threshold) {
            return;
        }
        let mark = if exit_code == 0 { "✓" } else { "✗" };
        let command = shortened_command(&block.command);
        let notification = SystemNotification {
            tag: self.notification_tag(id),
            title: format!("{mark} {command}").into(),
            body: format!("{} · exit {exit_code}", format_duration(duration)).into(),
            actions: Vec::new(),
        };
        cx.show_system_notification(notification);
    }

    fn notification_tag(&self, id: BlockId) -> gpui::SharedString {
        format!(
            "{NOTIFICATION_TAG_PREFIX}{}-{}",
            self.terminal.entity_id().as_u64(),
            id.0
        )
        .into()
    }
}

fn has_line_numbers(insight: &Insight) -> bool {
    matches!(insight, Insight::GitStatus { .. } | Insight::GitLog { .. })
}

fn is_located(recognised: &Recognised, block: &Block, grid_columns: usize) -> bool {
    recognised.columns == grid_columns && block.output_line == Some(recognised.output_line)
}

fn shortened_command(command: &str) -> String {
    if command.chars().count() > NOTIFIED_COMMAND_CHARS {
        let cut: String = command.chars().take(NOTIFIED_COMMAND_CHARS).collect();
        format!("{cut}…")
    } else {
        command.to_string()
    }
}

/// Reads the complete lines of a running block that `scan` has not seen yet, within its budget.
fn read_new_lines(
    terminal: &Terminal,
    id: BlockId,
    scan: &mut UrlScan,
    tail_lines: Option<usize>,
) -> Option<Vec<OutputLine>> {
    let output_line = terminal.blocks()?.block(id)?.output_line?;
    if scan.output_line != output_line {
        scan.output_line = output_line;
        scan.next_line = None;
    }
    let remaining = URL_SCAN_LINE_LIMIT.saturating_sub(scan.lines);
    if remaining == 0 {
        return None;
    }
    let lines = terminal.block_new_output_lines(
        id,
        scan.next_line,
        tail_lines,
        remaining,
        MAX_INSIGHT_BYTES,
    )?;
    let last = lines.last()?;
    scan.lines += lines.len();
    scan.next_line = Some(last.absolute_line + 1);
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, TestAppContext, VisualTestContext};
    use settings::SettingsStore;

    const CWD: &str = "\x1b]7;file://host/tmp\x07";

    fn command(command: &str, output: &str) -> String {
        format!(
            "{CWD}\x1b]133;A\x07$ \x1b]133;B\x07{command}\r\n\x1b]633;E;{command}\x07\x1b]133;C\x07{output}\x1b]133;D;1\x07"
        )
    }

    fn prompt() -> &'static str {
        "\x1b]133;A\x07$ \x1b]133;B\x07"
    }

    fn setup(
        cx: &mut TestAppContext,
        smart_blocks: bool,
    ) -> (
        Entity<Terminal>,
        Entity<SmartBlocks>,
        &mut VisualTestContext,
    ) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.set_app_identity("photon", "Photon");
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.terminal.get_or_insert_default().smart_blocks = Some(smart_blocks);
                });
            });
        });
        let terminal = cx.new(|cx| {
            let mut terminal = terminal::TerminalBuilder::new_display_only(
                terminal::terminal_settings::CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                util::paths::PathStyle::local(),
            )
            .subscribe(cx);
            terminal.enable_blocks_for_test(cx);
            terminal
        });
        let cx = cx.add_empty_window();
        cx.deactivate_window();
        let smart_blocks = cx.update(|window, cx| {
            cx.new(|cx| SmartBlocks::new(terminal.clone(), WeakEntity::new_invalid(), window, cx))
        });
        (terminal, smart_blocks, cx)
    }

    fn feed(terminal: &Entity<Terminal>, text: &str, cx: &mut VisualTestContext) {
        terminal.update(cx, |terminal, cx| {
            terminal.write_raw_output(text.as_bytes(), cx)
        });
        cx.run_until_parked();
        terminal.update(cx, |terminal, cx| terminal.process_wakeup_for_test(cx));
        cx.run_until_parked();
    }

    fn block_ids(terminal: &Entity<Terminal>, cx: &mut VisualTestContext) -> Vec<BlockId> {
        terminal.read_with(cx, |terminal, _| {
            terminal
                .blocks()
                .map(|blocks| blocks.blocks().map(|block| block.id).collect())
                .unwrap_or_default()
        })
    }

    const PERF_COMMANDS: usize = 1_000;
    const PERF_LINES_PER_COMMAND: usize = 100;
    const PERF_RUNS: usize = 15;
    const STREAM_LINES: usize = 100_000;
    // About one 64 KiB PTY read of short lines, which is what each Wakeup sees under load.
    const STREAM_CHUNK_LINES: usize = 2_000;

    /// CPU time of the calling thread, which runs the whole test app: time spent
    /// preempted on a busy machine does not count.
    fn thread_cpu_time() -> Duration {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `time` is a valid, writable timespec for the duration of the call.
        let result = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
        assert_eq!(result, 0, "clock_gettime failed");
        Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
    }

    type Check = dyn Fn(&SmartBlocks, BlockId);

    /// What recognition cost on the background executor, which the test thread ran while parked:
    /// the same reader and `recognise` the production task runs, timed over the retained finished
    /// blocks and scaled to every block that finished. Subtracted so the figure is the UI thread's.
    fn background_recognition_time(
        terminal: &Entity<Terminal>,
        reads: &[String],
        cx: &mut VisualTestContext,
    ) -> Duration {
        let finished: usize = reads.iter().map(|read| read.matches("133;D").count()).sum();
        terminal.read_with(cx, |terminal, _| {
            let Some(blocks) = terminal.blocks() else {
                return Duration::ZERO;
            };
            let started = thread_cpu_time();
            let mut measured = 0;
            for block in blocks.blocks().filter(|block| block.end_line.is_some()) {
                let Some(read_lines) =
                    terminal.block_output_reader(block.id, MAX_INSIGHT_LINES, MAX_INSIGHT_BYTES)
                else {
                    continue;
                };
                if let Some((lines, _)) = read_lines() {
                    recognise(&block.command, &lines, block.exit_code);
                }
                measured += 1;
            }
            (thread_cpu_time() - started) * finished as u32 / measured.max(1)
        })
    }

    /// Feeds each read (parked, because shell marks reach the terminal through its event channel,
    /// which also runs recognition on the test thread and overstates the UI thread's cost) and a
    /// Wakeup to a display-only terminal and returns the foreground CPU
    /// time. "On" is blocks plus `terminal.smart_blocks` plus a SmartBlocks entity (in an inactive
    /// window); "off" is `terminal.blocks: false`.
    /// `check` sees the newest block after the run, so a test cannot pass without the work.
    fn feed_reads(
        cx: &mut TestAppContext,
        with_smart_blocks: bool,
        reads: &[String],
        check: &Check,
    ) -> Duration {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.set_app_identity("photon", "Photon");
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let terminal = settings.terminal.get_or_insert_default();
                    terminal.smart_blocks = Some(with_smart_blocks);
                    terminal.blocks = Some(with_smart_blocks);
                });
            });
        });
        let terminal = cx.new(|cx| {
            let mut terminal = terminal::TerminalBuilder::new_display_only(
                terminal::terminal_settings::CursorShape::default(),
                terminal::terminal_settings::AlternateScroll::On,
                Some(10_000),
                0,
                cx.background_executor(),
                util::paths::PathStyle::local(),
            )
            .subscribe(cx);
            if with_smart_blocks {
                terminal.enable_blocks_for_test(cx);
            }
            terminal
        });
        let cx = cx.add_empty_window();
        cx.deactivate_window();
        let smart_blocks = with_smart_blocks.then(|| {
            cx.update(|window, cx| {
                cx.new(|cx| {
                    SmartBlocks::new(terminal.clone(), WeakEntity::new_invalid(), window, cx)
                })
            })
        });
        let started = thread_cpu_time();
        for read in reads {
            feed(&terminal, read, cx);
        }
        let elapsed = thread_cpu_time() - started;
        let background = if with_smart_blocks {
            background_recognition_time(&terminal, reads, cx)
        } else {
            Duration::ZERO
        };
        let elapsed = elapsed.saturating_sub(background);
        if let Some(smart_blocks) = &smart_blocks {
            let last = block_ids(&terminal, cx).last().copied();
            let last = last.expect("a block");
            smart_blocks.read_with(cx, |smart_blocks, _| check(smart_blocks, last));
        }
        elapsed
    }

    /// The fastest run is the least disturbed by other load on the machine.
    fn fastest(durations: Vec<Duration>) -> Duration {
        durations.into_iter().min().unwrap_or_default()
    }

    /// (blocks off, smart blocks on), interleaved so a burst of load hits both sides.
    fn measure_overhead(
        cx: &mut TestAppContext,
        reads: &[String],
        check: &Check,
    ) -> (Duration, Duration) {
        let mut without = Vec::new();
        let mut with = Vec::new();
        for _ in 0..PERF_RUNS {
            without.push(feed_reads(cx, false, reads, check));
            with.push(feed_reads(cx, true, reads, check));
        }
        (fastest(without), fastest(with))
    }

    fn assert_within_10_percent(label: &str, without: Duration, with: Duration) {
        eprintln!("{label}: blocks off {without:?}, smart blocks on {with:?}");
        assert!(
            with.as_secs_f64() <= without.as_secs_f64() * 1.10,
            "{label}: smart blocks on {with:?} vs blocks off {without:?}"
        );
    }

    /// 100,000 lines as 1,000 `pytest` commands, each finished in the same read as its output.
    fn command_reads() -> Vec<String> {
        (0..PERF_COMMANDS)
            .map(|_| {
                let mut output = "lorem ipsum dolor sit amet\r\n".repeat(PERF_LINES_PER_COMMAND);
                output.push_str("== 1 failed, 1 passed in 0.02s ==\r\n");
                command("pytest", &output)
            })
            .collect()
    }

    fn assert_finished_path_ran(smart_blocks: &SmartBlocks, last: BlockId) {
        assert!(smart_blocks.insight(last).is_some(), "Finished path ran");
    }

    #[gpui::test]
    async fn smart_blocks_per_command_overhead_under_1ms(cx: &mut TestAppContext) {
        let (without, with) = measure_overhead(cx, &command_reads(), &assert_finished_path_ran);
        let per_command_secs = (with.as_secs_f64() - without.as_secs_f64()) / PERF_COMMANDS as f64;
        eprintln!(
            "100,000 lines as commands: blocks off {without:?}, smart blocks on {with:?}; \
             per-command UI-thread overhead {:.1} us",
            per_command_secs * 1e6
        );
        assert!(per_command_secs < 0.001, "{per_command_secs}s per command");
    }

    /// One command streaming 100,000 lines with a Wakeup per 2,000, still running until the end,
    /// so the URL and error scans run on every Wakeup.
    fn streaming_reads(start_output: &str) -> Vec<String> {
        let mut reads = vec![format!(
            "{CWD}\x1b]133;A\x07$ \x1b]133;B\x07npm run dev\r\n\x1b]633;E;npm run dev\x07\x1b]133;C\x07{start_output}"
        )];
        for chunk in 0..STREAM_LINES / STREAM_CHUNK_LINES {
            let mut text = String::new();
            for line in 0..STREAM_CHUNK_LINES {
                let number = chunk * STREAM_CHUNK_LINES + line;
                text.push_str(&format!("{number} lorem ipsum dolor sit amet\r\n"));
            }
            reads.push(text);
        }
        reads.push("\x1b]133;D;0\x07".to_string());
        reads
    }

    /// No URL anywhere: the URL scan reads up to its budget, and the error scan must stay idle.
    #[gpui::test]
    async fn smart_blocks_streaming_url_scan_within_10_percent(cx: &mut TestAppContext) {
        let (without, with) = measure_overhead(
            cx,
            &streaming_reads("starting\r\n"),
            &|smart_blocks, last| {
                assert!(smart_blocks.local_url(last).is_none());
                let scan = smart_blocks.scans.get(&last).expect("URL scan ran");
                assert_eq!(scan.lines, URL_SCAN_LINE_LIMIT, "URL scan used its budget");
            },
        );
        assert_within_10_percent("100,000 streamed lines, URL scan", without, with);
    }

    /// A URL up front makes it a dev server in an inactive window, so the error scan runs on
    /// every Wakeup, reading only the tail of each 2,000-line chunk.
    #[gpui::test]
    async fn smart_blocks_streaming_error_scan_within_10_percent(cx: &mut TestAppContext) {
        let (without, with) = measure_overhead(
            cx,
            &streaming_reads("Local: http://localhost:5173/\r\n"),
            &|smart_blocks, last| {
                assert_eq!(smart_blocks.local_url(last), Some("http://localhost:5173/"));
                let scan = smart_blocks.error_scans.get(&last).expect("error scan ran");
                assert!(
                    scan.next_line
                        .is_some_and(|next| next - scan.output_line > 90_000),
                    "error scan reached the end of the output"
                );
            },
        );
        assert_within_10_percent("100,000 streamed lines, error scan", without, with);
    }

    #[gpui::test]
    async fn finished_test_block_gets_insight(cx: &mut TestAppContext) {
        let (terminal, smart, cx) = setup(cx, true);
        feed(
            &terminal,
            &command(
                "pytest",
                "FAILED t.py::a - x\r\n== 1 failed, 1 passed in 0.02s ==\r\n",
            ),
            cx,
        );
        let id = block_ids(&terminal, cx)[0];
        smart.read_with(cx, |smart, _| {
            assert!(matches!(smart.insight(id), Some(Insight::Tests(run)) if run.failed == 1));
        });
    }

    #[gpui::test]
    async fn insight_for_cleared_block_is_dropped(cx: &mut TestAppContext) {
        let (terminal, smart, cx) = setup(cx, true);
        feed(
            &terminal,
            &command(
                "pytest",
                "FAILED t.py::a - x\r\n== 1 failed, 1 passed in 0.02s ==\r\n",
            ),
            cx,
        );
        let id = block_ids(&terminal, cx)[0];
        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.clear();
                terminal.sync(window, cx);
            })
        });
        feed(&terminal, prompt(), cx);
        smart.read_with(cx, |smart, _| assert!(smart.insight(id).is_none()));
    }

    #[gpui::test]
    async fn second_run_gets_delta(cx: &mut TestAppContext) {
        let (terminal, smart, cx) = setup(cx, true);
        feed(
            &terminal,
            &command(
                "pytest",
                "FAILED t.py::a - x\r\n== 1 failed, 1 passed in 0.02s ==\r\n",
            ),
            cx,
        );
        feed(
            &terminal,
            &command("pytest", "== 2 passed in 0.02s ==\r\n"),
            cx,
        );
        let ids = block_ids(&terminal, cx);
        smart.read_with(cx, |smart, _| {
            assert!(smart.delta(ids[0]).is_none());
            let delta = smart.delta(ids[1]).expect("delta");
            assert_eq!(delta.fixed, ["t.py::a"]);
        });
    }

    #[gpui::test]
    async fn running_block_url_found(cx: &mut TestAppContext) {
        let (terminal, smart, cx) = setup(cx, true);
        feed(
            &terminal,
            &format!(
                "{CWD}\x1b]133;A\x07$ \x1b]133;B\x07npm run dev\r\n\x1b]633;E;npm run dev\x07\x1b]133;C\x07starting\r\n  Local: http://localhost:5173/\r\n"
            ),
            cx,
        );
        let id = block_ids(&terminal, cx)[0];
        smart.read_with(cx, |smart, _| {
            assert_eq!(smart.local_url(id), Some("http://localhost:5173/"));
        });
    }

    #[gpui::test]
    async fn url_scan_stops_after_limit(cx: &mut TestAppContext) {
        let (terminal, smart, cx) = setup(cx, true);
        feed(
            &terminal,
            &format!(
                "{CWD}\x1b]133;A\x07$ \x1b]133;B\x07serve\r\n\x1b]633;E;serve\x07\x1b]133;C\x07{}",
                "noise\r\n".repeat(URL_SCAN_LINE_LIMIT)
            ),
            cx,
        );
        feed(&terminal, "http://localhost:3000\r\n", cx);
        let id = block_ids(&terminal, cx)[0];
        smart.read_with(cx, |smart, _| assert_eq!(smart.local_url(id), None));
    }

    #[gpui::test]
    async fn smart_blocks_setting_off_records_nothing(cx: &mut TestAppContext) {
        let (terminal, smart, cx) = setup(cx, false);
        feed(
            &terminal,
            &command(
                "pytest",
                "FAILED t.py::a - x\r\n== 1 failed, 1 passed in 0.02s ==\r\n",
            ),
            cx,
        );
        let id = block_ids(&terminal, cx)[0];
        smart.read_with(cx, |smart, _| assert!(smart.insight(id).is_none()));
    }

    const START_SERVER: &str =
        "\x1b]133;A\x07$ \x1b]133;B\x07serve\r\n\x1b]633;E;serve\x07\x1b]133;C\x07";

    #[gpui::test]
    async fn url_split_across_reads_is_not_truncated(cx: &mut TestAppContext) {
        let (terminal, smart, cx) = setup(cx, true);
        feed(
            &terminal,
            &format!("{CWD}{START_SERVER}Local: http://localhost:51"),
            cx,
        );
        let id = block_ids(&terminal, cx)[0];
        smart.read_with(cx, |smart, _| assert_eq!(smart.local_url(id), None));
        feed(&terminal, "73/\r\n", cx);
        smart.read_with(cx, |smart, _| {
            assert_eq!(smart.local_url(id), Some("http://localhost:5173/"));
        });
    }

    #[gpui::test]
    async fn url_inside_a_burst_over_the_budget_is_found(cx: &mut TestAppContext) {
        let (terminal, smart, cx) = setup(cx, true);
        let noise = "noise\r\n".repeat(URL_SCAN_LINE_LIMIT - 500);
        feed(
            &terminal,
            &format!("{CWD}{START_SERVER}{noise}http://localhost:3000\r\n{noise}"),
            cx,
        );
        let id = block_ids(&terminal, cx)[0];
        smart.read_with(cx, |smart, _| {
            assert_eq!(smart.local_url(id), Some("http://localhost:3000"));
        });
    }

    #[gpui::test]
    async fn block_cleared_while_recognising_stores_nothing(cx: &mut TestAppContext) {
        let (terminal, smart, cx) = setup(cx, true);
        let window = cx.windows()[0];
        let _clear_on_finish = cx.update(|_, cx| {
            let clearing = terminal.clone();
            cx.subscribe(&terminal, move |_, event: &Event, cx| {
                if matches!(event, Event::BlocksChanged(Some(BlockEvent::Finished(_)))) {
                    window
                        .update(cx, |_, window, cx| {
                            clearing.update(cx, |terminal, cx| {
                                terminal.clear();
                                terminal.sync(window, cx);
                            })
                        })
                        .log_err();
                }
            })
        });
        feed(
            &terminal,
            &command(
                "pytest",
                "FAILED t.py::a - x\r\n== 1 failed, 1 passed in 0.02s ==\r\n",
            ),
            cx,
        );
        assert!(block_ids(&terminal, cx).is_empty());
        smart.read_with(cx, |smart, _| assert!(smart.insights.is_empty()));
    }

    mod notifications {
        use super::*;
        use std::time::Duration;

        fn notify_after(seconds: u64, cx: &mut VisualTestContext) {
            cx.update(|_, cx| {
                cx.update_global::<SettingsStore, _>(|store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings
                            .terminal
                            .get_or_insert_default()
                            .notify_after_seconds = Some(seconds);
                    });
                });
            });
        }

        fn run_for_over_a_second(
            terminal: &Entity<Terminal>,
            cx: &mut VisualTestContext,
        ) -> BlockId {
            feed(terminal, &format!("{CWD}{START_SERVER}"), cx);
            // Block durations are measured with the real clock.
            std::thread::sleep(Duration::from_millis(1100));
            feed(terminal, "\x1b]133;D;0\x07", cx);
            block_ids(terminal, cx)[0]
        }

        #[gpui::test]
        async fn long_command_notifies_when_inactive(cx: &mut TestAppContext) {
            let (terminal, _smart, cx) = setup(cx, true);
            notify_after(1, cx);
            let id = run_for_over_a_second(&terminal, cx);
            let shown = cx.shown_system_notifications();
            assert_eq!(shown.len(), 1);
            assert_eq!(shown[0].title, "✓ serve");
            assert!(shown[0].body.ends_with(" · exit 0"), "{}", shown[0].body);
            assert_eq!(
                shown[0].tag,
                format!("photon-block-{}-{}", terminal.entity_id().as_u64(), id.0)
            );
        }

        #[gpui::test]
        async fn failed_command_title_is_cut_at_sixty_characters(cx: &mut TestAppContext) {
            let (terminal, _smart, cx) = setup(cx, true);
            notify_after(1, cx);
            let long = "x".repeat(70);
            feed(
                &terminal,
                &format!(
                    "{CWD}\x1b]133;A\x07$ \x1b]133;B\x07{long}\r\n\x1b]633;E;{long}\x07\x1b]133;C\x07"
                ),
                cx,
            );
            std::thread::sleep(Duration::from_millis(1100));
            feed(&terminal, "\x1b]133;D;2\x07", cx);
            let shown = cx.shown_system_notifications();
            assert_eq!(shown[0].title, format!("✗ {}…", "x".repeat(60)));
            assert!(shown[0].body.ends_with(" · exit 2"));
        }

        #[gpui::test]
        async fn short_or_active_does_not_notify(cx: &mut TestAppContext) {
            let (terminal, _smart, cx) = setup(cx, true);
            notify_after(1, cx);
            feed(&terminal, &command("ls", ""), cx);
            assert!(cx.shown_system_notifications().is_empty());

            cx.update(|window, _| window.activate_window());
            cx.run_until_parked();
            cx.update(|window, _| assert!(window.is_window_active()));
            feed(&terminal, prompt(), cx);
            run_for_over_a_second(&terminal, cx);
            assert!(cx.shown_system_notifications().is_empty());
        }

        #[gpui::test]
        async fn notify_setting_zero_disables(cx: &mut TestAppContext) {
            let (terminal, _smart, cx) = setup(cx, true);
            notify_after(0, cx);
            run_for_over_a_second(&terminal, cx);
            assert!(cx.shown_system_notifications().is_empty());
        }

        #[gpui::test]
        async fn server_error_notifies_once_per_minute(cx: &mut TestAppContext) {
            let (terminal, _smart, cx) = setup(cx, true);
            feed(
                &terminal,
                &format!("{CWD}{START_SERVER}Local: http://localhost:3000\r\n"),
                cx,
            );
            assert!(cx.shown_system_notifications().is_empty());
            feed(&terminal, "  error: boom\r\n", cx);
            let shown = cx.shown_system_notifications();
            assert_eq!(shown.len(), 1);
            assert_eq!(shown[0].title, "Error in serve");
            assert_eq!(shown[0].body, "error: boom");

            feed(&terminal, "error: again\r\n", cx);
            assert_eq!(cx.shown_system_notifications().len(), 1);

            cx.executor().advance_clock(Duration::from_secs(61));
            cx.run_until_parked();
            feed(&terminal, "error: later\r\n", cx);
            let shown = cx.shown_system_notifications();
            assert_eq!(shown.len(), 2);
            assert_eq!(shown[1].body, "error: later");
        }

        #[gpui::test]
        async fn error_scan_checks_only_the_end_of_a_burst(cx: &mut TestAppContext) {
            let (terminal, _smart, cx) = setup(cx, true);
            feed(
                &terminal,
                &format!("{CWD}{START_SERVER}Local: http://localhost:3000\r\n"),
                cx,
            );
            let burst = |error_at: usize| {
                (0..1000)
                    .map(|line| match line == error_at {
                        true => "error: in the burst\r\n".to_string(),
                        false => format!("{line} fine\r\n"),
                    })
                    .collect::<String>()
            };
            feed(&terminal, &burst(10), cx);
            assert!(
                cx.shown_system_notifications().is_empty(),
                "an error far before the end of a burst is skipped"
            );
            feed(&terminal, &burst(950), cx);
            let shown = cx.shown_system_notifications();
            assert_eq!(shown.len(), 1);
            assert_eq!(shown[0].body, "error: in the burst");
        }

        #[gpui::test]
        async fn smart_blocks_off_does_not_notify(cx: &mut TestAppContext) {
            let (terminal, _smart, cx) = setup(cx, false);
            notify_after(1, cx);
            run_for_over_a_second(&terminal, cx);
            feed(
                &terminal,
                &format!("{CWD}{START_SERVER}Local: http://localhost:3000\r\nerror: boom\r\n"),
                cx,
            );
            assert!(cx.shown_system_notifications().is_empty());
        }

        #[gpui::test]
        async fn error_seen_while_active_is_not_reported_later(cx: &mut TestAppContext) {
            let (terminal, _smart, cx) = setup(cx, true);
            cx.update(|window, _| window.activate_window());
            cx.run_until_parked();
            feed(
                &terminal,
                &format!("{CWD}{START_SERVER}Local: http://localhost:3000\r\nerror: seen\r\n"),
                cx,
            );
            cx.deactivate_window();
            feed(&terminal, "fine\r\n", cx);
            assert!(cx.shown_system_notifications().is_empty());
            feed(&terminal, "error: new\r\n", cx);
            let shown = cx.shown_system_notifications();
            assert_eq!(shown.len(), 1);
            assert_eq!(shown[0].body, "error: new");
        }

        #[gpui::test]
        async fn server_error_after_many_lines_still_notifies(cx: &mut TestAppContext) {
            let (terminal, _smart, cx) = setup(cx, true);
            feed(
                &terminal,
                &format!("{CWD}{START_SERVER}Local: http://localhost:3000\r\n"),
                cx,
            );
            for _ in 0..3 {
                feed(&terminal, &"noise\r\n".repeat(URL_SCAN_LINE_LIMIT), cx);
            }
            assert!(cx.shown_system_notifications().is_empty());
            feed(&terminal, "error: late\r\n", cx);
            let shown = cx.shown_system_notifications();
            assert_eq!(shown.len(), 1);
            assert_eq!(shown[0].body, "error: late");
        }

        #[gpui::test]
        async fn server_error_while_active_is_silent(cx: &mut TestAppContext) {
            let (terminal, _smart, cx) = setup(cx, true);
            cx.update(|window, _| window.activate_window());
            cx.run_until_parked();
            feed(
                &terminal,
                &format!("{CWD}{START_SERVER}Local: http://localhost:3000\r\nerror: boom\r\n"),
                cx,
            );
            assert!(cx.shown_system_notifications().is_empty());
        }

        #[gpui::test]
        async fn response_asks_the_view_to_focus_the_block(cx: &mut TestAppContext) {
            let (terminal, smart, cx) = setup(cx, true);
            let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let _subscription = cx.update(|_, cx| {
                let events = events.clone();
                cx.subscribe(&smart, move |_, event: &SmartBlocksEvent, _| {
                    if let SmartBlocksEvent::FocusBlock(id) = event {
                        events.borrow_mut().push(*id);
                    }
                })
            });
            feed(&terminal, &command("ls", ""), cx);
            let id = block_ids(&terminal, cx)[0];
            cx.simulate_system_notification_response(gpui::SystemNotificationResponse {
                tag: format!("photon-block-{}-{}", terminal.entity_id().as_u64(), id.0).into(),
                action_id: None,
            });
            cx.run_until_parked();
            assert_eq!(*events.borrow(), [id]);
        }
    }

    mod git_actions {
        use super::*;
        use project::Project;
        use std::path::Path;
        use workspace::{AppState, MultiWorkspace, notifications::NotificationId};

        async fn git(directory: &Path, args: &[&str]) {
            let status = util::command::new_command("git")
                .arg("-C")
                .arg(directory)
                .args(args)
                .status()
                .await
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        }

        async fn git_stdout(directory: &Path, args: &[&str]) -> String {
            let output = util::command::new_command("git")
                .arg("-C")
                .arg(directory)
                .args(args)
                .output()
                .await
                .expect("run git");
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        /// A repository with committed `a.txt` and `a b.txt`, both modified since.
        async fn repository() -> Option<tempfile::TempDir> {
            which::which("git").ok()?;
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path();
            git(path, &["init", "-b", "main"]).await;
            git(path, &["config", "user.name", "Test"]).await;
            git(path, &["config", "user.email", "test@example.com"]).await;
            std::fs::write(path.join("a.txt"), "one").expect("write");
            std::fs::write(path.join("a b.txt"), "one").expect("write");
            git(path, &["add", "."]).await;
            git(path, &["commit", "-m", "init"]).await;
            std::fs::write(path.join("a.txt"), "two").expect("write");
            std::fs::write(path.join("a b.txt"), "two").expect("write");
            Some(directory)
        }

        async fn setup_with_workspace(
            cx: &mut TestAppContext,
        ) -> (
            Entity<Terminal>,
            Entity<SmartBlocks>,
            Entity<Workspace>,
            VisualTestContext,
        ) {
            let app_state = cx.update(AppState::test);
            cx.update(|cx| {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
                cx.update_global::<SettingsStore, _>(|store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings.terminal.get_or_insert_default().smart_blocks = Some(true);
                    });
                });
            });
            let project = Project::test(app_state.fs.clone(), [], cx).await;
            let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
            let workspace = window
                .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
                .expect("workspace");
            let terminal = cx.new(|cx| {
                let mut terminal = terminal::TerminalBuilder::new_display_only(
                    terminal::terminal_settings::CursorShape::default(),
                    terminal::terminal_settings::AlternateScroll::On,
                    None,
                    0,
                    cx.background_executor(),
                    util::paths::PathStyle::local(),
                )
                .subscribe(cx);
                terminal.enable_blocks_for_test(cx);
                terminal
            });
            let smart_blocks = window
                .update(cx, |_, window, cx| {
                    cx.new(|cx| {
                        SmartBlocks::new(terminal.clone(), workspace.downgrade(), window, cx)
                    })
                })
                .expect("window");
            let cx = VisualTestContext::from_window(window.into(), cx);
            (terminal, smart_blocks, workspace, cx)
        }

        fn status_block(directory: &Path, output: &str) -> String {
            format!(
                "\x1b]7;file://host{}\x07\x1b]133;A\x07$ \x1b]133;B\x07git status -s\r\n\x1b]633;E;git status -s\x07\x1b]133;C\x07{output}\x1b]133;D;0\x07",
                directory.display()
            )
        }

        fn status_files(
            smart: &Entity<SmartBlocks>,
            id: BlockId,
            cx: &mut VisualTestContext,
        ) -> Vec<photon_blocks::StatusFile> {
            smart.read_with(cx, |smart, _| match smart.insight(id) {
                Some(Insight::GitStatus { files }) => files.clone(),
                _ => Vec::new(),
            })
        }

        /// Git runs as a real process, so wait for it in real time.
        fn wait_until(
            cx: &mut VisualTestContext,
            mut done: impl FnMut(&mut VisualTestContext) -> bool,
        ) {
            for _ in 0..500 {
                cx.run_until_parked();
                if done(cx) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("timed out waiting for git");
        }

        #[gpui::test]
        async fn stage_runs_git_add_and_marks_row(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let Some(directory) = repository().await else {
                return;
            };
            let path = directory.path();
            let (terminal, smart, _workspace, mut cx) = setup_with_workspace(cx).await;
            feed(&terminal, &status_block(path, " M a.txt\r\n"), &mut cx);
            let id = block_ids(&terminal, &mut cx)[0];
            let files = status_files(&smart, id, &mut cx);
            let line = files[0].absolute_line;
            smart.update(&mut cx, |smart, cx| smart.stage(id, line, cx));
            wait_until(&mut cx, |cx| {
                smart.read_with(cx, |smart, _| smart.row_mark(id, "a.txt").is_some())
            });
            smart.read_with(&cx, |smart, _| {
                assert!(matches!(smart.row_mark(id, "a.txt"), Some(RowMark::Staged)));
            });
            assert_eq!(
                git_stdout(path, &["diff", "--cached", "--name-only"]).await,
                "a.txt\n"
            );
        }

        #[gpui::test]
        async fn porcelain_paths_are_staged_from_the_repository_root(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let Some(directory) = repository().await else {
                return;
            };
            let path = directory.path();
            let subdirectory = path.join("sub");
            std::fs::create_dir(&subdirectory).expect("mkdir");
            let (terminal, smart, _workspace, mut cx) = setup_with_workspace(cx).await;
            let block = status_block(&subdirectory, " M a.txt\r\n")
                .replace("git status -s", "git status --porcelain");
            feed(&terminal, &block, &mut cx);
            let id = block_ids(&terminal, &mut cx)[0];
            let line = status_files(&smart, id, &mut cx)[0].absolute_line;
            smart.update(&mut cx, |smart, cx| smart.stage(id, line, cx));
            wait_until(&mut cx, |cx| {
                smart.read_with(cx, |smart, _| smart.row_mark(id, "a.txt").is_some())
            });
            assert_eq!(
                git_stdout(path, &["diff", "--cached", "--name-only"]).await,
                "a.txt\n"
            );
        }

        #[gpui::test]
        async fn stage_quotes_path(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let Some(directory) = repository().await else {
                return;
            };
            let path = directory.path();
            let (terminal, smart, _workspace, mut cx) = setup_with_workspace(cx).await;
            feed(
                &terminal,
                &status_block(path, " M \"a b.txt\"\r\n"),
                &mut cx,
            );
            let id = block_ids(&terminal, &mut cx)[0];
            let files = status_files(&smart, id, &mut cx);
            assert_eq!(files[0].path, "a b.txt");
            let line = files[0].absolute_line;
            smart.update(&mut cx, |smart, cx| smart.stage(id, line, cx));
            wait_until(&mut cx, |cx| {
                smart.read_with(cx, |smart, _| smart.row_mark(id, "a b.txt").is_some())
            });
            assert_eq!(
                git_stdout(path, &["diff", "--cached", "--name-only"]).await,
                "a b.txt\n"
            );
        }

        #[gpui::test]
        async fn discard_requires_confirmation(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let Some(directory) = repository().await else {
                return;
            };
            let path = directory.path();
            let (terminal, smart, _workspace, mut cx) = setup_with_workspace(cx).await;
            feed(&terminal, &status_block(path, " M a.txt\r\n"), &mut cx);
            let id = block_ids(&terminal, &mut cx)[0];
            let line = status_files(&smart, id, &mut cx)[0].absolute_line;

            smart.update_in(&mut cx, |smart, window, cx| {
                smart.discard(id, line, window, cx)
            });
            cx.run_until_parked();
            assert_eq!(
                cx.pending_prompt().map(|(message, _)| message),
                Some("Discard changes to a.txt?".to_string())
            );
            cx.simulate_prompt_answer("Cancel");
            cx.run_until_parked();
            assert_eq!(
                std::fs::read_to_string(path.join("a.txt")).expect("read"),
                "two"
            );
            smart.read_with(&cx, |smart, _| {
                assert!(smart.row_mark(id, "a.txt").is_none())
            });

            smart.update_in(&mut cx, |smart, window, cx| {
                smart.discard(id, line, window, cx)
            });
            cx.run_until_parked();
            cx.simulate_prompt_answer("Discard");
            wait_until(&mut cx, |cx| {
                smart.read_with(cx, |smart, _| smart.row_mark(id, "a.txt").is_some())
            });
            smart.read_with(&cx, |smart, _| {
                assert!(matches!(
                    smart.row_mark(id, "a.txt"),
                    Some(RowMark::Discarded)
                ));
            });
            assert_eq!(
                std::fs::read_to_string(path.join("a.txt")).expect("read"),
                "one"
            );
        }

        #[gpui::test]
        async fn git_failure_shows_toast(cx: &mut TestAppContext) {
            cx.executor().allow_parking();
            let Some(directory) = repository().await else {
                return;
            };
            let path = directory.path();
            let (terminal, smart, workspace, mut cx) = setup_with_workspace(cx).await;
            feed(
                &terminal,
                &status_block(path, " M missing.txt\r\n"),
                &mut cx,
            );
            let id = block_ids(&terminal, &mut cx)[0];
            let line = status_files(&smart, id, &mut cx)[0].absolute_line;
            smart.update(&mut cx, |smart, cx| smart.stage(id, line, cx));
            let toast = NotificationId::unique::<SmartBlocks>();
            wait_until(&mut cx, |cx| {
                workspace.read_with(cx, |workspace, _| workspace.has_notification(&toast))
            });
            smart.read_with(&cx, |smart, _| {
                assert!(smart.row_mark(id, "missing.txt").is_none())
            });
        }
    }
}
