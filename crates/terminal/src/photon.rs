use std::time::{Duration, Instant};

use alacritty_terminal::{grid::Dimensions as _, index::Line, term::cell::Flags};
use collections::{HashMap, HashSet};
use gpui::{Context, Task};
use photon_blocks::{BlockEvent, BlockId, BlockList, Phase, ShellMark, parse_shell_mark};
use util::ResultExt as _;

use crate::{
    Event, Terminal,
    alacritty::{
        AlacrittyTerm, absolute_lines_text, history_clears, is_alt_screen, oldest_absolute_line,
        top_absolute_line, used_lines,
    },
};

const MAX_BLOCKS: usize = 1000;
const NO_MARKS_TIMEOUT: Duration = Duration::from_secs(2);
/// The last field of every mark Photon's zsh hooks print (assets/shell_integration/zsh).
const PHOTON_MARK_TAG: &[u8] = b"photon";

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
    /// Set once a command is sent, until the shell answers with a prompt (A) or output (C).
    /// Until then zsh may be at a continuation prompt (an unclosed quote), which only the
    /// PTY can answer.
    awaiting_shell: bool,
    /// Set once Photon's own zsh hooks were heard (their marks end in a `photon` field). From
    /// then on untagged marks come from a program inside a block (an agent CLI, another
    /// integration) and are ignored rather than read as the shell misbehaving.
    tagged_marks: bool,
    /// The command that was running when the screen was cleared (`clear` itself, usually).
    /// If it ends without printing anything after the clear, it leaves no block behind.
    cleared_while_running: Option<BlockId>,
}

impl TerminalBlocks {
    fn new(max_blocks: usize, history_clears: u64) -> Self {
        Self {
            list: BlockList::new(max_blocks),
            no_marks_check: None,
            history_clears,
            oldest_line: 0,
            awaiting_shell: false,
            tagged_marks: false,
            cleared_while_running: None,
        }
    }

    /// cmd-K: Zed's clear moves the cursor line to the top even when there was
    /// no history to clear, so it cannot be detected from the grid alone. On the
    /// alternate screen it clears only that screen, so the blocks stay.
    pub(crate) fn clear(&mut self, term: &AlacrittyTerm) {
        if is_alt_screen(term) {
            return;
        }
        self.clear_list();
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
            self.clear_list();
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
        let is_cwd = params
            .first()
            .is_some_and(|first| first.starts_with(b"file://"));
        let params = match params.split_last() {
            Some((last, rest)) if !is_cwd && last.as_slice() == PHOTON_MARK_TAG => {
                self.tagged_marks = true;
                rest
            }
            _ if !is_cwd && self.tagged_marks => return None,
            _ => params,
        };
        let mark = parse_shell_mark(params)?;
        if matches!(mark, ShellMark::PromptStart | ShellMark::OutputStart) {
            self.awaiting_shell = false;
        }
        Some(self.list.apply(mark, absolute_line, Instant::now()))
    }

    /// How many logical lines (wrapped rows joined) each mark line is above the cursor's line.
    /// Re-wrapping keeps this distance, so it finds the marks again after a width change.
    pub(crate) fn distances_from_cursor(&self, term: &AlacrittyTerm) -> HashMap<i64, usize> {
        let wanted: HashSet<i64> = self.list.mark_lines().into_iter().collect();
        let mut distances = HashMap::default();
        walk_logical_lines_up(term, |absolute_line, distance, _| {
            if wanted.contains(&absolute_line) {
                distances.entry(absolute_line).or_insert(distance);
            }
        });
        distances
    }

    /// Puts each mark on the first row of the logical line it was on before the grid re-wrapped.
    pub(crate) fn restore_from_cursor(
        &mut self,
        term: &AlacrittyTerm,
        distances: &HashMap<i64, usize>,
    ) {
        let mut starts: HashMap<usize, i64> = HashMap::default();
        walk_logical_lines_up(term, |absolute_line, distance, is_start| {
            if is_start {
                starts.insert(distance, absolute_line);
            }
        });
        self.list.remap_lines(|line| {
            distances
                .get(&line)
                .and_then(|distance| starts.get(distance).copied())
        });
        self.oldest_line = oldest_absolute_line(term).unwrap_or(self.oldest_line);
    }

    fn clear_list(&mut self) {
        self.cleared_while_running = self.list.running();
        self.list.clear();
    }

    /// Called after the grid may have changed under the block list.
    pub(crate) fn sync_with_grid(&mut self, term: &AlacrittyTerm) {
        let clears = history_clears(term);
        if clears != self.history_clears {
            self.history_clears = clears;
            self.oldest_line = 0;
            self.clear_list();
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
            no_marks_check: Some(no_marks_check),
            ..TerminalBlocks::new(MAX_BLOCKS, history_clears(&self.term.lock()))
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
        let Some(event) = blocks.apply_shell_mark(params, absolute_line, history_clears) else {
            return;
        };
        if let Some(BlockEvent::Finished(id)) = event
            && blocks.cleared_while_running.take() == Some(id)
            && self
                .block_text(id, BlockPart::Output)
                .is_some_and(|text| text.trim().is_empty())
            && let Some(blocks) = &mut self.blocks
        {
            blocks.list.remove(id);
        }
        cx.emit(Event::BlocksChanged(event));
    }

    /// None when shell integration is off.
    pub fn blocks(&self) -> Option<&BlockList> {
        self.blocks.as_ref().map(|blocks| &blocks.list)
    }

    pub fn input_target(&self) -> InputTarget {
        match &self.blocks {
            Some(blocks)
                if *blocks.list.phase() == Phase::Prompt
                    && !blocks.awaiting_shell
                    && !is_alt_screen(&self.term.lock()) =>
            {
                InputTarget::BlockInput
            }
            _ => InputTarget::Pty,
        }
    }

    /// Sends the command as one bracketed paste plus Enter, so multi-line text runs as a single
    /// command. Does nothing unless the shell is waiting at a block prompt, where the text
    /// would otherwise be typed into a running program or joined to an unfinished command.
    pub fn run_command(&mut self, command: &str, cx: &mut Context<Self>) {
        if self.input_target() != InputTarget::BlockInput {
            return;
        }
        // ESC is dropped (as in `paste`) so the text cannot end the paste early.
        let command = command.replace('\x1b', "");
        // Ctrl-U first clears typeahead left in zsh's line buffer, which the paste would join.
        self.input(format!("\x15\x1b[200~{command}\x1b[201~\r").into_bytes());
        if let Some(blocks) = &mut self.blocks {
            blocks.awaiting_shell = true;
        }
        cx.emit(Event::BlocksChanged(None));
    }

    /// The grid's width, which moves to a new size only when the resize is processed.
    pub fn grid_columns(&self) -> usize {
        self.term.lock().columns()
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

    /// Logical lines of the block's output (wrapped rows joined), each with the absolute line it
    /// starts on. None when the output is larger than `max_lines` or `max_bytes`.
    pub fn block_output_lines(
        &self,
        id: BlockId,
        max_lines: usize,
        max_bytes: usize,
    ) -> Option<Vec<photon_blocks::OutputLine>> {
        self.block_output_reader(id, max_lines, max_bytes)
            .and_then(|read| read())
            .map(|(lines, _)| lines)
    }

    /// The same read as `block_output_lines`, deferred: the closure walks the grid when called,
    /// so a background task can do the copying instead of the UI thread. It reads the lines the
    /// block had when this was called; the caller must check the block still exists afterwards.
    pub fn block_output_reader(
        &self,
        id: BlockId,
        max_lines: usize,
        max_bytes: usize,
    ) -> Option<impl FnOnce() -> Option<(Vec<photon_blocks::OutputLine>, usize)> + Send + use<>>
    {
        let read = OutputRead {
            output_line: self.blocks()?.block(id)?.output_line?,
            end_line: self.blocks()?.block(id)?.end_line,
            from_line: None,
            tail_lines: None,
            completed_only: false,
            truncate: false,
            max_lines,
            max_bytes,
        };
        let term = self.term.clone();
        // The column count comes from the same lock as the rows, so a caller can tell whether the
        // grid re-wrapped after it captured `output_line`.
        Some(move || {
            let term = term.lock();
            Some((read_output_lines(&term, &read)?, term.columns()))
        })
    }

    /// Complete logical lines of a running block that start at or after `from_line`, at most
    /// `max_lines` of them (the rest are left unread); with `tail_lines`, only the last that many. The line holding the cursor is still
    /// being printed, so it is left for a later read.
    pub fn block_new_output_lines(
        &self,
        id: BlockId,
        from_line: Option<i64>,
        tail_lines: Option<usize>,
        max_lines: usize,
        max_bytes: usize,
    ) -> Option<Vec<photon_blocks::OutputLine>> {
        let block = self.blocks()?.block(id)?;
        let read = OutputRead {
            output_line: block.output_line?,
            end_line: block.end_line,
            from_line,
            tail_lines,
            completed_only: true,
            truncate: true,
            max_lines,
            max_bytes,
        };
        read_output_lines(&self.term.lock(), &read)
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

    /// Blank screen rows below the cursor while nothing has scrolled into history; drawing the
    /// grid that much lower keeps the newest output next to the input box, as Warp does.
    pub fn blank_rows_below_cursor(&self) -> usize {
        let term = self.term.lock();
        if self.blocks.is_none() || term.grid().display_offset() > 0 || is_alt_screen(&term) {
            return 0;
        }
        // Counted below the last row with text as well as the cursor, so nothing is pushed off
        // screen. After `clear` the rows above are history, which scrolling still reaches.
        let used_rows = used_lines(&term) - term.history_size();
        term.screen_lines().saturating_sub(used_rows)
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

struct OutputRead {
    output_line: i64,
    end_line: Option<i64>,
    from_line: Option<i64>,
    /// Read only this many of the last logical lines; the earlier ones are skipped uncopied.
    tail_lines: Option<usize>,
    /// Stop before the logical line holding the cursor.
    completed_only: bool,
    /// Stop at the limits with what was read, instead of returning None.
    truncate: bool,
    max_lines: usize,
    max_bytes: usize,
}

const TAB_WIDTH: usize = 8;

fn read_output_lines(
    term: &AlacrittyTerm,
    read: &OutputRead,
) -> Option<Vec<photon_blocks::OutputLine>> {
    if is_alt_screen(term) {
        return None;
    }
    let origin = term.absolute_line(Line(0));
    let grid = term.grid();
    let last_column = grid.last_column();
    let output_start = read.output_line.max(read.from_line.unwrap_or(i64::MIN));
    let mut first = Line(i32::try_from(output_start - origin).ok()?).max(grid.topmost_line());
    let mut last = match read.end_line {
        Some(end) => Line(i32::try_from(end - origin - 1).ok()?),
        None => grid.cursor.point.line,
    }
    .min(grid.bottommost_line());
    if read.completed_only {
        let mut cursor_line_start = grid.cursor.point.line;
        while cursor_line_start > grid.topmost_line() {
            let above: Line = cursor_line_start - 1;
            if !grid[above][last_column].flags.contains(Flags::WRAPLINE) {
                break;
            }
            cursor_line_start = above;
        }
        last = last.min(cursor_line_start - 1);
    }
    // Start at a logical line, not in the middle of a wrapped one.
    while read.from_line.is_some() && first > grid.topmost_line() && first <= last {
        let above: Line = first - 1;
        if !grid[above][last_column].flags.contains(Flags::WRAPLINE) {
            break;
        }
        first += 1;
    }

    if let Some(tail) = read.tail_lines {
        let mut starts = 0;
        let mut row: Line = last;
        while row >= first {
            let above: Line = row - 1;
            if row <= grid.topmost_line()
                || !grid[above][last_column].flags.contains(Flags::WRAPLINE)
            {
                starts += 1;
                if starts == tail {
                    first = row;
                    break;
                }
            }
            row -= 1;
        }
    }
    let mut lines: Vec<photon_blocks::OutputLine> = Vec::new();
    let mut bytes = 0;
    let mut continuing = false;
    let mut line = first;
    while line <= last {
        let row = &grid[line];
        let wraps = row[last_column].flags.contains(Flags::WRAPLINE);
        let cells = &row[alacritty_terminal::index::Column(0)
            ..alacritty_terminal::index::Column(grid.columns())];
        // Trailing blanks are trimmed anyway unless the row continues on the next one.
        let cells = if wraps {
            cells
        } else {
            let used = cells
                .iter()
                .rposition(|cell| cell.c != ' ')
                .map_or(0, |last| last + 1);
            &cells[..used]
        };
        let mut text = String::with_capacity(cells.len());
        let mut in_tab = false;
        for (column, cell) in cells.iter().enumerate() {
            // A TAB leaves a '\t' cell followed by blank cells up to the next tab stop; read it
            // back as the single '\t' the program printed, as alacritty's own copy does.
            // ponytail: assumes the default 8-column tab stops; alacritty keeps custom stops
            // private.
            // Spacer cells are blank, so the flag check only matters for blanks.
            if cell.c == ' ' {
                if in_tab && column % TAB_WIDTH != 0 {
                    continue;
                }
                in_tab = false;
                if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    text.push(' ');
                }
            } else {
                in_tab = cell.c == '\t';
                text.push(cell.c);
            }
        }
        match lines.last_mut().filter(|_| continuing) {
            Some(open) => open.text.push_str(&text),
            None => lines.push(photon_blocks::OutputLine {
                absolute_line: term.absolute_line(line),
                text,
            }),
        }
        continuing = wraps;
        if !wraps && let Some(done) = lines.last_mut() {
            done.text.truncate(done.text.trim_end().len());
            bytes += done.text.len();
            if lines.len() > read.max_lines || bytes > read.max_bytes {
                if !read.truncate {
                    return None;
                }
                lines.pop();
                break;
            }
        }
        line += 1;
    }
    // A wrapped row left open at the end of the range still needs its trailing spaces trimmed.
    if let Some(open) = lines.last_mut().filter(|_| continuing) {
        open.text.truncate(open.text.trim_end().len());
    }
    while lines.last().is_some_and(|last| last.text.is_empty()) {
        lines.pop();
    }
    Some(lines)
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
        let mut blocks = with_blocks.then(|| TerminalBlocks::new(MAX_BLOCKS, 0));
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

    /// Polls the real PTY until `done` holds, or fails after about 5 s.
    #[cfg(target_os = "macos")]
    async fn wait_until(
        terminal: &gpui::Entity<Terminal>,
        what: &str,
        cx: &mut gpui::TestAppContext,
        done: impl Fn(&Terminal) -> bool,
    ) {
        for _ in 0..500 {
            if terminal.read_with(cx, |terminal, _| done(terminal)) {
                return;
            }
            cx.background_executor
                .timer(Duration::from_millis(10))
                .await;
        }
        let content = terminal.read_with(cx, |terminal, _| terminal.get_content());
        panic!("timed out waiting for {what}:\n{content}");
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    async fn unclosed_quote_waits_for_shell_until_ctrl_c(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        cx.update_global(|store: &mut settings::SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().blocks = Some(true);
            });
        });
        // An empty home, so the developer's own zsh config stays out of the test.
        let home = std::env::temp_dir().join(format!("photon-quote-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).expect("create home");
        let home_text = home.to_string_lossy().into_owned();
        let env = collections::HashMap::from_iter([
            ("HOME".to_string(), home_text.clone()),
            ("ZDOTDIR".to_string(), home_text),
        ]);
        let builder = cx
            .update(|cx| {
                crate::TerminalBuilder::new(
                    None,
                    crate::TerminalMode::interactive(),
                    task::Shell::WithArguments {
                        program: "/bin/zsh".to_string(),
                        args: vec!["-i".to_string()],
                        title_override: None,
                    },
                    env,
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
        let at_prompt = |terminal: &Terminal| terminal.input_target() == InputTarget::BlockInput;
        wait_until(&terminal, "the first prompt", cx, at_prompt).await;

        terminal.update(cx, |terminal, cx| terminal.run_command("echo \"it is", cx));
        wait_until(&terminal, "the continuation prompt", cx, |terminal| {
            terminal.get_content().contains("dquote>")
        })
        .await;
        terminal.read_with(cx, |terminal, _| {
            assert_eq!(terminal.input_target(), InputTarget::Pty)
        });

        terminal.update(cx, |terminal, _| terminal.input(b"\x03".as_slice()));
        wait_until(&terminal, "the prompt after ctrl-c", cx, at_prompt).await;
        terminal.update(cx, |terminal, cx| terminal.run_command("echo SECOND", cx));
        let finished = |terminal: &Terminal| {
            terminal.blocks().is_some_and(|blocks| {
                blocks
                    .blocks()
                    .any(|block| block.end_line.is_some() && block.command == "echo SECOND")
            })
        };
        wait_until(&terminal, "the second command", cx, finished).await;
        terminal.read_with(cx, |terminal, _| {
            let blocks = terminal.blocks().expect("blocks");
            let commands: Vec<_> = blocks.blocks().map(|block| block.command.clone()).collect();
            assert_eq!(commands, ["echo SECOND"]);
            let id = blocks.blocks().next().map(|block| block.id).expect("block");
            assert_eq!(
                terminal.block_text(id, BlockPart::Output).as_deref(),
                Some("SECOND")
            );
        });
        std::fs::remove_dir_all(&home).log_err();
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

/// Walks rows from the cursor's row up to the oldest row in history, calling `visit` with each
/// row's absolute line, its distance in logical lines from the cursor's line, and whether it
/// starts a logical line (the row above it did not wrap into it).
fn walk_logical_lines_up(term: &AlacrittyTerm, mut visit: impl FnMut(i64, usize, bool)) {
    let grid = term.grid();
    let last_column = grid.last_column();
    let top = grid.topmost_line();
    let mut distance = 0;
    let mut line = grid.cursor.point.line;
    loop {
        let above: Line = line - 1;
        let starts_line = line == top || !grid[above][last_column].flags.contains(Flags::WRAPLINE);
        visit(term.absolute_line(line), distance, starts_line);
        if line == top {
            break;
        }
        if starts_line {
            distance += 1;
        }
        line = line - 1;
    }
}
