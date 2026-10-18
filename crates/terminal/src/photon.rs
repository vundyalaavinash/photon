use std::time::{Duration, Instant};

use gpui::{Context, Task};
use photon_blocks::{BlockId, BlockList, Phase, parse_shell_mark};
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
        let absolute_line = blocks.line_after_clears(absolute_line, history_clears);
        let Some(mark) = parse_shell_mark(params) else {
            return;
        };
        let event = blocks.list.apply(mark, absolute_line, Instant::now());
        cx.emit(Event::BlocksChanged(event));
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
        self.input(format!("\x1b[200~{command}\x1b[201~\r").into_bytes());
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

    pub fn set_block_branch(&mut self, branch: Option<String>) {
        if let Some(blocks) = &mut self.blocks {
            blocks.list.set_branch(branch);
        }
    }
}
