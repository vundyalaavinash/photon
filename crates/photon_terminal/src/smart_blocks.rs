use collections::HashMap;
use gpui::{
    AppContext as _, Context, Entity, EventEmitter, Subscription, Task, WeakEntity, Window,
};
use photon_blocks::{
    BlockEvent, BlockId, Insight, MAX_INSIGHT_BYTES, MAX_INSIGHT_LINES, RunDelta, compare_runs,
    find_local_url, recognise,
};
use settings::Settings as _;
use terminal::{Event, Terminal, terminal_settings::TerminalSettings};
use util::ResultExt as _;
use workspace::Workspace;

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

/// What finished and running command blocks turned out to be, computed off the UI thread.
pub struct SmartBlocks {
    terminal: Entity<Terminal>,
    _workspace: WeakEntity<Workspace>,
    insights: HashMap<BlockId, Insight>,
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
            _workspace: workspace,
            insights: HashMap::default(),
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
}
