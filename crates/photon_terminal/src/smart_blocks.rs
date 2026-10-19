use std::path::PathBuf;

use anyhow::{Context as _, Result};

use collections::HashMap;
use git_ui::{commit_view::CommitView, project_diff::ProjectDiff};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, PromptLevel, Subscription, Task,
    WeakEntity, Window,
};
use photon_blocks::{
    BlockEvent, BlockId, Insight, MAX_INSIGHT_BYTES, MAX_INSIGHT_LINES, RunDelta, StatusFile,
    compare_runs, find_local_url, recognise,
};
use settings::Settings as _;
use terminal::{Event, Terminal, terminal_settings::TerminalSettings};
use util::{ResultExt as _, paths::normalize_lexically};
use workspace::{Toast, Workspace, notifications::NotificationId};

use crate::chips::run_git;

/// How many output lines of a running block are searched for a local URL.
const URL_SCAN_LINE_LIMIT: usize = 2_000;

pub enum SmartBlocksEvent {
    Changed,
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

/// What finished and running command blocks turned out to be, computed off the UI thread.
pub struct SmartBlocks {
    terminal: Entity<Terminal>,
    workspace: WeakEntity<Workspace>,
    insights: HashMap<BlockId, Insight>,
    row_marks: HashMap<(BlockId, i64), RowMark>,
    deltas: HashMap<BlockId, RunDelta>,
    urls: HashMap<BlockId, String>,
    scans: HashMap<BlockId, UrlScan>,
    tasks: HashMap<BlockId, Task<()>>,
    _terminal_subscription: Subscription,
}

impl EventEmitter<SmartBlocksEvent> for SmartBlocks {}

impl SmartBlocks {
    pub fn new(
        terminal: Entity<Terminal>,
        workspace: WeakEntity<Workspace>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let _terminal_subscription = cx.subscribe(&terminal, Self::handle_terminal_event);
        Self {
            terminal,
            workspace,
            insights: HashMap::default(),
            row_marks: HashMap::default(),
            deltas: HashMap::default(),
            urls: HashMap::default(),
            scans: HashMap::default(),
            tasks: HashMap::default(),
            _terminal_subscription,
        }
    }

    pub fn insight(&self, id: BlockId) -> Option<&Insight> {
        self.insights.get(&id)
    }

    pub fn delta(&self, id: BlockId) -> Option<&RunDelta> {
        self.deltas.get(&id)
    }

    pub fn local_url(&self, id: BlockId) -> Option<&str> {
        self.urls.get(&id).map(String::as_str)
    }

    pub fn row_mark(&self, id: BlockId, absolute_line: i64) -> Option<RowMark> {
        self.row_marks.get(&(id, absolute_line)).copied()
    }

    pub fn stage(&mut self, id: BlockId, absolute_line: i64, cx: &mut Context<Self>) {
        let Some(file) = self.status_file(id, absolute_line) else {
            return;
        };
        let path = file.path.clone();
        self.run_git_action(
            id,
            vec![absolute_line],
            ["add", "--", &path],
            RowMark::Staged,
            cx,
        );
    }

    pub fn stage_all(&mut self, id: BlockId, cx: &mut Context<Self>) {
        let Some(Insight::GitStatus { files }) = self.insights.get(&id) else {
            return;
        };
        let lines = files.iter().map(|file| file.absolute_line).collect();
        self.run_git_action(id, lines, ["add", "-A"], RowMark::Staged, cx);
    }

    /// Asks first: `git restore` throws the changes away for good.
    pub fn discard(
        &mut self,
        id: BlockId,
        absolute_line: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.status_file(id, absolute_line) else {
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
                    vec![absolute_line],
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
            self.status_file(id, absolute_line),
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

    fn status_file(&self, id: BlockId, absolute_line: i64) -> Option<&StatusFile> {
        let Some(Insight::GitStatus { files }) = self.insights.get(&id) else {
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
        lines: Vec<i64>,
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
                    for line in lines {
                        this.row_marks.insert((id, line), mark);
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
                    self.recognise_block(*id, cx);
                }
            }
            Event::Wakeup if enabled => self.scan_running_block_for_url(cx),
            _ => {}
        }
    }

    fn forget_removed_blocks(&mut self, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        let Some(blocks) = terminal.blocks() else {
            return;
        };
        let kept = |id: &BlockId| blocks.block(*id).is_some();
        let before = self.insights.len() + self.urls.len();
        self.insights.retain(|id, _| kept(id));
        self.deltas.retain(|id, _| kept(id));
        self.urls.retain(|id, _| kept(id));
        self.scans.retain(|id, _| kept(id));
        self.tasks.retain(|id, _| kept(id));
        self.row_marks.retain(|(id, _), _| kept(id));
        if self.insights.len() + self.urls.len() != before {
            cx.emit(SmartBlocksEvent::Changed);
        }
    }

    fn recognise_block(&mut self, id: BlockId, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        let Some(block) = terminal.blocks().and_then(|blocks| blocks.block(id)) else {
            return;
        };
        let command = block.command.clone();
        let exit_code = block.exit_code;
        let Some(read_lines) =
            terminal.block_output_reader(id, MAX_INSIGHT_LINES, MAX_INSIGHT_BYTES)
        else {
            return;
        };
        let task = cx.background_spawn(async move {
            let lines = read_lines()?;
            recognise(&command, &lines, exit_code)
        });
        let task = cx.spawn(async move |this, cx| {
            let insight = task.await;
            this.update(cx, |this, cx| {
                this.tasks.remove(&id);
                this.store_insight(id, insight, cx);
            })
            .log_err();
        });
        self.tasks.insert(id, task);
    }

    fn store_insight(&mut self, id: BlockId, insight: Option<Insight>, cx: &mut Context<Self>) {
        let exists = self
            .terminal
            .read(cx)
            .blocks()
            .is_some_and(|blocks| blocks.block(id).is_some());
        let Some(insight) = insight.filter(|_| exists) else {
            return;
        };
        if let Insight::Tests(run) = &insight
            && let Some(delta) = self
                .previous_run(id, cx)
                .map(|previous| compare_runs(previous, run))
        {
            self.deltas.insert(id, delta);
        }
        self.insights.insert(id, insight);
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
            .find_map(|block| match self.insights.get(&block.id) {
                Some(Insight::Tests(run)) => Some(run),
                _ => None,
            })
    }

    fn scan_running_block_for_url(&mut self, cx: &mut Context<Self>) {
        let terminal = self.terminal.read(cx);
        let Some(id) = terminal.blocks().and_then(|blocks| blocks.running()) else {
            return;
        };
        if self.urls.contains_key(&id) {
            return;
        }
        let Some(output_line) = terminal
            .blocks()
            .and_then(|blocks| blocks.block(id))
            .and_then(|block| block.output_line)
        else {
            return;
        };
        let scan = self.scans.entry(id).or_default();
        if scan.output_line != output_line {
            scan.output_line = output_line;
            scan.next_line = None;
        }
        let remaining = URL_SCAN_LINE_LIMIT.saturating_sub(scan.lines);
        if remaining == 0 {
            return;
        }
        let Some(lines) =
            terminal.block_new_output_lines(id, scan.next_line, remaining, MAX_INSIGHT_BYTES)
        else {
            return;
        };
        let Some(last) = lines.last() else {
            return;
        };
        scan.lines += lines.len();
        scan.next_line = Some(last.absolute_line + 1);
        if let Some(url) = lines.iter().find_map(|line| find_local_url(&line.text)) {
            self.urls.insert(id, url);
            cx.emit(SmartBlocksEvent::Changed);
        }
    }
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
                smart.read_with(cx, |smart, _| smart.row_mark(id, line).is_some())
            });
            smart.read_with(&cx, |smart, _| {
                assert!(matches!(smart.row_mark(id, line), Some(RowMark::Staged)));
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
                smart.read_with(cx, |smart, _| smart.row_mark(id, line).is_some())
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
                smart.read_with(cx, |smart, _| smart.row_mark(id, line).is_some())
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
            smart.read_with(&cx, |smart, _| assert!(smart.row_mark(id, line).is_none()));

            smart.update_in(&mut cx, |smart, window, cx| {
                smart.discard(id, line, window, cx)
            });
            cx.run_until_parked();
            cx.simulate_prompt_answer("Discard");
            wait_until(&mut cx, |cx| {
                smart.read_with(cx, |smart, _| smart.row_mark(id, line).is_some())
            });
            smart.read_with(&cx, |smart, _| {
                assert!(matches!(smart.row_mark(id, line), Some(RowMark::Discarded)));
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
            smart.read_with(&cx, |smart, _| assert!(smart.row_mark(id, line).is_none()));
        }
    }
}
