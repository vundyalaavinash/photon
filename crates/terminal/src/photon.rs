use std::time::{Duration, Instant};

use gpui::{Context, Task};
use photon_blocks::{BlockEvent, BlockId, BlockList, Phase, parse_shell_mark};
use util::ResultExt as _;

use crate::{
    Event, Terminal,
    alacritty::{
        AlacrittyTerm, absolute_lines_text, history_clears, is_alt_screen, oldest_absolute_line,
        top_absolute_line,
    },
};

const MAX_BLOCKS: usize = 1000;
const NO_MARKS_TIMEOUT: Duration = Duration::from_secs(2);

/// Where keyboard input should go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputTarget {
    BlockInput,
    Pty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockPart {
    Command,
    Output,
    Both,
}

pub(crate) struct TerminalBlocks {
    list: BlockList,
    /// Dropping the task cancels the check, which happens on the first mark.
    no_marks_check: Option<Task<()>>,
    history_clears: u64,
    oldest_line: i64,
}

impl TerminalBlocks {
    /// cmd-K: Zed's clear moves the cursor line to the top even when there was
    /// no history to clear, so it cannot be detected from the grid alone.
    pub(crate) fn clear(&mut self, term: &AlacrittyTerm) {
        self.list.clear();
        self.sync_with_grid(term);
    }

    /// Marks are numbered as of the clear count when the parser saw them, which can
    /// lag the grid by a frame: a clear the list has not seen yet is applied first,
    /// and a mark from before a clear the list already applied lands on line 0,
    /// where that clear left the cursor.
    fn line_after_clears(&mut self, absolute_line: i64, mark_clears: u64) -> i64 {
        if mark_clears > self.history_clears {
            self.history_clears = mark_clears;
            self.oldest_line = 0;
            self.list.clear();
        }
        if mark_clears < self.history_clears {
            0
        } else {
            absolute_line
        }
    }

    /// None when the params are not a mark; otherwise what the mark did to the list.
    fn apply_shell_mark(
        &mut self,
        params: &[Vec<u8>],
        absolute_line: i64,
        history_clears: u64,
    ) -> Option<Option<BlockEvent>> {
        let absolute_line = self.line_after_clears(absolute_line, history_clears);
        let mark = parse_shell_mark(params)?;
        Some(self.list.apply(mark, absolute_line, Instant::now()))
    }

    /// Called after the grid may have changed under the block list.
    pub(crate) fn sync_with_grid(&mut self, term: &AlacrittyTerm) {
        let clears = history_clears(term);
        if clears != self.history_clears {
            self.history_clears = clears;
            self.oldest_line = 0;
            self.list.clear();
        }
        // The oldest retained line only moves once history is full and evicting.
        if let Some(oldest) = oldest_absolute_line(term)
            && oldest > self.oldest_line
        {
            self.oldest_line = oldest;
            self.list.trim_before(oldest);
        }
    }
}

impl Terminal {
    pub(crate) fn enable_blocks(&mut self, cx: &Context<Self>) {
        let no_marks_check = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NO_MARKS_TIMEOUT).await;
            this.update(cx, |this, cx| {
                if let Some(blocks) = &mut this.blocks {
                    blocks.list.degrade("no shell marks");
                    cx.emit(Event::BlocksChanged(None));
                }
            })
            .log_err();
        });
        self.blocks = Some(TerminalBlocks {
            list: BlockList::new(MAX_BLOCKS),
            no_marks_check: Some(no_marks_check),
            history_clears: history_clears(&self.term.lock()),
            oldest_line: 0,
        });
    }

    pub(crate) fn apply_shell_mark(
        &mut self,
        params: &[Vec<u8>],
        absolute_line: i64,
        history_clears: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(blocks) = &mut self.blocks else {
            return;
        };
        blocks.no_marks_check = None;
        if let Some(event) = blocks.apply_shell_mark(params, absolute_line, history_clears) {
            cx.emit(Event::BlocksChanged(event));
        }
    }

    /// None when shell integration is off.
    pub fn blocks(&self) -> Option<&BlockList> {
        self.blocks.as_ref().map(|blocks| &blocks.list)
    }

    pub fn input_target(&self) -> InputTarget {
        match &self.blocks {
            Some(blocks)
                if *blocks.list.phase() == Phase::Prompt && !is_alt_screen(&self.term.lock()) =>
            {
                InputTarget::BlockInput
            }
            _ => InputTarget::Pty,
        }
    }

    /// Sends the command as one bracketed paste plus Enter, so multi-line text runs as a single command.
    pub fn run_command(&mut self, command: &str) {
        // ESC is dropped (as in `paste`) so the text cannot end the paste early.
        let command = command.replace('\x1b', "");
        // Ctrl-U first clears typeahead left in zsh's line buffer, which the paste would join.
        self.input(format!("\x15\x1b[200~{command}\x1b[201~\r").into_bytes());
    }

    /// Absolute line of the first visible row.
    pub fn top_absolute_line(&self) -> i64 {
        top_absolute_line(&self.term.lock())
    }

    pub fn block_text(&self, id: BlockId, part: BlockPart) -> Option<String> {
        let block = self.blocks()?.block(id)?;
        let output = || absolute_lines_text(&self.term.lock(), block.output_line?, block.end_line);
        match part {
            BlockPart::Command => Some(block.command.clone()),
            BlockPart::Output => output(),
            BlockPart::Both => Some(format!("{}\n{}", block.command, output()?)),
        }
    }

    /// Grid lines from the block's header through its last output line (to the cursor while
    /// running); history is negative.
    pub fn block_grid_lines(&self, id: BlockId) -> Option<std::ops::RangeInclusive<i32>> {
        let block = self.blocks()?.block(id)?;
        let term = self.term.lock();
        let origin = term.absolute_line(alacritty_terminal::index::Line(0));
        let first = i32::try_from(block.prompt_line - origin).ok()?;
        let last = match block.end_line {
            Some(end) => i32::try_from(end - origin - 1).ok()?,
            None => term.grid().cursor.point.line.0,
        };
        Some(first..=last)
    }

    pub fn set_block_branch(&mut self, branch: Option<String>) {
        if let Some(blocks) = &mut self.blocks {
            blocks.list.set_branch(branch);
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn enable_blocks_for_test(&mut self, cx: &Context<Self>) {
        self.enable_blocks(cx);
    }

    /// The PTY event loop's Wakeup, which display-only terminals never receive.
    #[cfg(any(test, feature = "test-support"))]
    pub fn process_wakeup_for_test(&mut self, cx: &mut Context<Self>) {
        self.process_event(crate::TerminalBackendEvent::Wakeup, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DEFAULT_SCROLL_HISTORY_LINES, PtyEvent, TerminalBackendEvent, TerminalBounds,
        alacritty::{new_term, pty_term_config},
        terminal_settings::{AlternateScroll, CursorShape},
    };
    use futures::channel::mpsc::unbounded;
    use gpui::{AppContext as _, bounds, point, px, size};
    use vte::ansi::{Processor, StdSyncHandler};

    const LINES: usize = 100_000;
    // Far more commands than a single `seq 100000`, so mark handling is not lost in the noise.
    const LINES_PER_COMMAND: usize = 100;
    const RUNS: usize = 9;
    // Roughly what one PTY read hands the parser while output streams.
    const READ_SIZE: usize = 64 * 1024;

    /// The same text either way; with marks, it is cut into commands by zsh-style OSC marks.
    #[cfg(unix)]
    fn shell_output(with_marks: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        for line in 0..LINES {
            if with_marks && line % LINES_PER_COMMAND == 0 {
                if line > 0 {
                    bytes.extend_from_slice(b"\x1b]133;D;0\x07");
                }
                bytes.extend_from_slice(
                    b"\x1b]7;file://host/tmp\x07\x1b]133;A\x07\x1b]133;B\x07\x1b]633;E;seq 100\x07\x1b]133;C\x07",
                );
            }
            bytes.extend_from_slice(format!("{line} lorem ipsum dolor sit amet\r\n").as_bytes());
        }
        if with_marks {
            bytes.extend_from_slice(b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07");
        }
        bytes
    }

    /// CPU time of the calling thread, so time spent preempted on a busy machine does not count.
    #[cfg(unix)]
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

    /// Mirrors the PTY loop: parse one read, handle the events it produced, then the Wakeup.
    #[cfg(unix)]
    fn print(bytes: &[u8], with_blocks: bool) -> Duration {
        let (events_tx, mut events_rx) = unbounded();
        let terminal_bounds = TerminalBounds::new(
            px(20.),
            px(10.),
            bounds(point(px(0.), px(0.)), size(px(800.), px(600.))),
        );
        let term = new_term(
            &pty_term_config(DEFAULT_SCROLL_HISTORY_LINES, CursorShape::default()),
            terminal_bounds,
            events_tx,
            AlternateScroll::On,
            0,
        );
        let mut blocks = with_blocks.then(|| TerminalBlocks {
            list: BlockList::new(MAX_BLOCKS),
            no_marks_check: None,
            history_clears: 0,
            oldest_line: 0,
        });
        let mut processor = Processor::<StdSyncHandler>::new();
        let started = thread_cpu_time();
        for read in bytes.chunks(READ_SIZE) {
            processor.advance(&mut *term.lock(), read);
            while let Ok(PtyEvent::Event(event, _)) = events_rx.try_recv() {
                if let (
                    Some(blocks),
                    TerminalBackendEvent::ShellIntegration {
                        params,
                        absolute_line,
                        history_clears,
                    },
                ) = (&mut blocks, event)
                {
                    blocks.apply_shell_mark(&params, absolute_line, history_clears);
                }
            }
            if let Some(blocks) = &mut blocks {
                blocks.sync_with_grid(&term.lock());
            }
        }
        let elapsed = thread_cpu_time() - started;
        if let Some(blocks) = &blocks {
            assert_eq!(blocks.list.phase(), &Phase::Prompt);
            // Only the blocks still in the 10,000-line scrollback survive.
            let retained = blocks.list.blocks().count();
            assert!((90..=110).contains(&retained), "{retained} blocks");
        }
        elapsed
    }

    #[cfg(unix)]
    fn median(mut durations: Vec<Duration>) -> Duration {
        durations.sort();
        durations[durations.len() / 2]
    }

    #[cfg(unix)]
    #[test]
    fn heavy_output_overhead_within_10_percent() {
        let plain = shell_output(false);
        let marked = shell_output(true);
        let mut without = Vec::new();
        let mut with = Vec::new();
        // Interleaved, so a burst of load on the machine hits both sides.
        for _ in 0..RUNS {
            without.push(print(&plain, false));
            with.push(print(&marked, true));
        }
        let (without, with) = (median(without), median(with));
        eprintln!("100,000 lines, thread CPU time: blocks off {without:?}, blocks on {with:?}");
        assert!(
            with.as_secs_f64() <= without.as_secs_f64() * 1.10,
            "blocks on {with:?} vs off {without:?}"
        );
    }

    /// A zsh started with `-f` skips Photon's startup files, so it reports the
    /// environment the terminal launched it with.
    #[cfg(target_os = "macos")]
    async fn launch_env_probe(blocks: bool, cx: &mut gpui::TestAppContext) -> String {
        cx.update_global(|store: &mut settings::SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().blocks = Some(blocks);
            });
        });
        let builder = cx
            .update(|cx| {
                crate::TerminalBuilder::new(
                    None,
                    crate::TerminalMode::interactive(),
                    task::Shell::WithArguments {
                        program: "/bin/zsh".to_string(),
                        args: vec![
                            "-f".to_string(),
                            "-c".to_string(),
                            "[[ $ZDOTDIR == */shell_integration/zsh ]] && echo zdotdir=photon || echo zdotdir=user; echo blocks=${PHOTON_BLOCKS-unset}".to_string(),
                        ],
                        title_override: None,
                    },
                    collections::HashMap::default(),
                    CursorShape::default(),
                    AlternateScroll::On,
                    None,
                    Vec::new(),
                    Duration::ZERO,
                    false,
                    0,
                    cx,
                    Vec::new(),
                    util::paths::PathStyle::local(),
                )
            })
            .await
            .expect("spawn zsh");
        let terminal = cx.new(|cx| builder.subscribe(cx));
        terminal.read_with(cx, |terminal, _| {
            assert_eq!(terminal.blocks().is_some(), blocks);
            assert_eq!(terminal.input_target(), InputTarget::Pty);
        });
        // The PTY thread fills the grid in real time.
        let mut content = String::new();
        for _ in 0..300 {
            content = terminal.update(cx, |terminal, _| terminal.get_content());
            if content.contains("blocks=") {
                break;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        content
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn blocks_setting_off_gives_classic_terminal(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let classic = launch_env_probe(false, cx).await;
        assert!(classic.contains("zdotdir=user"), "{classic}");
        assert!(classic.contains("blocks=unset"), "{classic}");

        // The same probe sees the integration when the setting is on.
        let integrated = launch_env_probe(true, cx).await;
        assert!(integrated.contains("zdotdir=photon"), "{integrated}");
        assert!(integrated.contains("blocks=1"), "{integrated}");
    }
}
