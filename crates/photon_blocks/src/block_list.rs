use crate::marks::ShellMark;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(pub u64);

#[derive(Clone, Debug)]
pub struct Block {
    pub id: BlockId,
    pub command: String,
    pub save_to_history: bool,
    pub cwd: Option<PathBuf>,
    pub branch: Option<String>,
    pub prompt_line: i64,
    /// Where the B mark put the typed command; at or below `prompt_line`.
    pub command_line: i64,
    pub output_line: Option<i64>,
    pub end_line: Option<i64>,
    pub started_at: Option<Instant>,
    pub duration: Option<Duration>,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    AwaitingFirstMark,
    Prompt,
    Running,
    Degraded(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockEvent {
    Started(BlockId),
    Finished(BlockId),
}

pub struct BlockList {
    blocks: VecDeque<Block>,
    max_blocks: usize,
    next_id: u64,
    phase: Phase,
    prompt_line: i64,
    command_line: i64,
    command_started: bool,
    pending_command: Option<(String, bool)>,
    current_cwd: Option<PathBuf>,
    branch: Option<String>,
}

impl BlockList {
    pub fn new(max_blocks: usize) -> Self {
        Self {
            blocks: VecDeque::new(),
            max_blocks,
            next_id: 0,
            phase: Phase::AwaitingFirstMark,
            prompt_line: 0,
            command_line: 0,
            command_started: false,
            pending_command: None,
            current_cwd: None,
            branch: None,
        }
    }

    pub fn apply(
        &mut self,
        mark: ShellMark,
        absolute_line: i64,
        now: Instant,
    ) -> Option<BlockEvent> {
        if matches!(self.phase, Phase::Degraded(_)) {
            return None;
        }
        match mark {
            ShellMark::Cwd(path) => self.current_cwd = Some(path),
            ShellMark::PromptStart => {
                if self.phase == Phase::Running {
                    self.degrade("prompt started while a command was running");
                } else {
                    self.phase = Phase::Prompt;
                    self.prompt_line = absolute_line;
                    self.command_started = false;
                    self.pending_command = None;
                }
            }
            ShellMark::CommandStart => {
                if self.phase == Phase::Prompt {
                    self.command_started = true;
                    self.command_line = absolute_line;
                }
            }
            ShellMark::CommandText {
                command,
                save_to_history,
            } => match self.phase {
                Phase::Prompt => self.pending_command = Some((command, save_to_history)),
                // Another integration's preexec ran before ours and already sent C.
                Phase::Running => {
                    if let Some(block) = self.blocks.back_mut()
                        && block.command.is_empty()
                    {
                        block.command = command;
                        block.save_to_history = save_to_history;
                    }
                }
                _ => {}
            },
            ShellMark::OutputStart => match self.phase {
                Phase::Prompt if self.command_started => {
                    return Some(self.start_block(absolute_line, now));
                }
                Phase::Running => {}
                _ => self.degrade("command output started without a command start"),
            },
            ShellMark::CommandEnd { exit_code } => {
                if self.phase == Phase::Running {
                    self.phase = Phase::Prompt;
                    self.command_started = false;
                    let block = self.blocks.back_mut()?;
                    block.end_line = Some(absolute_line);
                    block.exit_code = exit_code;
                    block.duration = block.started_at.map(|started| now.duration_since(started));
                    return Some(BlockEvent::Finished(block.id));
                }
            }
        }
        None
    }

    fn start_block(&mut self, absolute_line: i64, now: Instant) -> BlockEvent {
        let (command, save_to_history) = self.pending_command.take().unwrap_or_default();
        let id = BlockId(self.next_id);
        self.next_id += 1;
        self.blocks.push_back(Block {
            id,
            command,
            save_to_history,
            cwd: self.current_cwd.clone(),
            branch: self.branch.clone(),
            prompt_line: self.prompt_line,
            command_line: self.command_line.max(self.prompt_line),
            output_line: Some(absolute_line),
            end_line: None,
            started_at: Some(now),
            duration: None,
            exit_code: None,
        });
        if self.blocks.len() > self.max_blocks {
            self.blocks.pop_front();
        }
        self.phase = Phase::Running;
        BlockEvent::Started(id)
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn degrade(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        log::warn!("terminal blocks disabled: {reason}");
        self.phase = Phase::Degraded(reason);
    }

    pub fn blocks(&self) -> impl DoubleEndedIterator<Item = &Block> {
        let visible = if matches!(self.phase, Phase::Degraded(_)) {
            0
        } else {
            self.blocks.len()
        };
        self.blocks.iter().take(visible)
    }

    pub fn remove(&mut self, id: BlockId) {
        self.blocks.retain(|block| block.id != id);
    }

    /// The block whose command is still running, if any.
    pub fn running(&self) -> Option<BlockId> {
        (self.phase == Phase::Running)
            .then(|| self.blocks.back().map(|block| block.id))
            .flatten()
    }

    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks().find(|block| block.id == id)
    }

    pub fn current_cwd(&self) -> Option<&Path> {
        self.current_cwd.as_deref()
    }

    pub fn set_branch(&mut self, branch: Option<String>) {
        self.branch = branch;
    }

    pub fn trim_before(&mut self, oldest_retained_line: i64) {
        while self
            .blocks
            .front()
            .is_some_and(|block| block.end_line.is_some_and(|end| end < oldest_retained_line))
        {
            self.blocks.pop_front();
        }
    }

    /// Every line a block or the pending prompt starts or ends on.
    pub fn mark_lines(&self) -> Vec<i64> {
        let mut lines = vec![self.prompt_line, self.command_line];
        for block in &self.blocks {
            lines.extend([block.prompt_line, block.command_line]);
            lines.extend(block.output_line);
            lines.extend(block.end_line);
        }
        lines
    }

    /// Moves every line through `map` after the grid re-wrapped; blocks with a line that no
    /// longer exists are dropped.
    pub fn remap_lines(&mut self, map: impl Fn(i64) -> Option<i64>) {
        self.prompt_line = map(self.prompt_line).unwrap_or(self.prompt_line);
        self.command_line = map(self.command_line).unwrap_or(self.command_line);
        self.blocks.retain_mut(|block| {
            let (Some(prompt), Some(command)) = (map(block.prompt_line), map(block.command_line))
            else {
                return false;
            };
            let output = block.output_line.map(&map);
            let end = block.end_line.map(&map);
            if output == Some(None) || end == Some(None) {
                return false;
            }
            block.prompt_line = prompt;
            block.command_line = command;
            block.output_line = output.flatten();
            block.end_line = end.flatten();
            true
        });
    }

    /// Drops finished blocks but keeps the phase, so a command running through
    /// cmd-K or `clear` still owns the keyboard and a pending prompt still runs.
    pub fn clear(&mut self) {
        self.blocks.retain(|block| block.end_line.is_none());
        // Clearing restarts absolute numbering with the cursor's line as line 0,
        // so whatever survives (prompt or running output) now starts there.
        self.prompt_line = 0;
        self.command_line = 0;
        for block in &mut self.blocks {
            block.prompt_line = 0;
            block.command_line = 0;
            block.output_line = Some(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marks::ShellMark::*;
    use std::{path::Path, time::Instant};

    fn text(command: &str) -> crate::marks::ShellMark {
        CommandText {
            command: command.into(),
            save_to_history: true,
        }
    }

    fn run(list: &mut BlockList, command: &str, line: i64, now: Instant) {
        list.apply(PromptStart, line, now);
        list.apply(CommandStart, line, now);
        list.apply(text(command), line, now);
        list.apply(OutputStart, line + 1, now);
        list.apply(CommandEnd { exit_code: Some(0) }, line + 2, now);
    }

    #[test]
    fn block_lifecycle() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        list.apply(Cwd("/tmp".into()), 0, now);
        list.apply(PromptStart, 0, now);
        list.apply(CommandStart, 0, now);
        list.apply(text("git status"), 0, now);
        assert!(matches!(
            list.apply(OutputStart, 1, now),
            Some(BlockEvent::Started(_))
        ));
        assert!(matches!(
            list.apply(CommandEnd { exit_code: Some(0) }, 3, now),
            Some(BlockEvent::Finished(_))
        ));
        list.apply(PromptStart, 3, now);
        let blocks: Vec<_> = list.blocks().collect();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].command, "git status");
        assert_eq!(blocks[0].cwd.as_deref(), Some(Path::new("/tmp")));
        assert_eq!(blocks[0].output_line, Some(1));
        assert_eq!(blocks[0].end_line, Some(3));
        assert_eq!(blocks[0].exit_code, Some(0));
        assert_eq!(list.phase(), &Phase::Prompt);
    }

    #[test]
    fn block_records_where_the_command_starts() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        list.apply(PromptStart, 5, now);
        list.apply(CommandStart, 7, now);
        list.apply(text("ls"), 7, now);
        list.apply(OutputStart, 8, now);
        let block = list.blocks().next().expect("block");
        assert_eq!((block.prompt_line, block.command_line), (5, 7));
        list.clear();
        let block = list.blocks().next().expect("running block survives clear");
        assert_eq!((block.prompt_line, block.command_line), (0, 0));
    }

    #[test]
    fn empty_enter_makes_no_block() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        list.apply(PromptStart, 0, now);
        list.apply(CommandStart, 0, now);
        list.apply(PromptStart, 1, now);
        assert_eq!(list.blocks().count(), 0);
        assert_eq!(list.phase(), &Phase::Prompt);
    }

    #[test]
    fn duplicate_prompt_marks_are_tolerated() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        for mark in [
            PromptStart,
            PromptStart,
            CommandStart,
            CommandStart,
            text("ls"),
            OutputStart,
            CommandEnd { exit_code: Some(0) },
        ] {
            list.apply(mark, 0, now);
        }
        assert_eq!(list.blocks().count(), 1);
        assert!(!matches!(list.phase(), Phase::Degraded(_)));
    }

    #[test]
    fn out_of_order_marks_degrade() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        list.apply(PromptStart, 0, now);
        list.apply(OutputStart, 1, now);
        assert!(matches!(list.phase(), Phase::Degraded(_)));

        let mut list = BlockList::new(10);
        list.apply(PromptStart, 0, now);
        list.apply(CommandStart, 0, now);
        list.apply(OutputStart, 1, now);
        list.apply(PromptStart, 2, now);
        assert!(matches!(list.phase(), Phase::Degraded(_)));
        run(&mut list, "ls", 3, now);
        assert_eq!(list.blocks().count(), 0);
    }

    #[test]
    fn trim_drops_blocks_before_oldest_line() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        run(&mut list, "one", 8, now);
        run(&mut list, "two", 48, now);
        list.trim_before(20);
        let commands: Vec<_> = list.blocks().map(|block| block.command.as_str()).collect();
        assert_eq!(commands, ["two"]);
    }

    #[test]
    fn cap_drops_oldest() {
        let now = Instant::now();
        let mut list = BlockList::new(2);
        for (index, command) in ["a", "b", "c"].iter().enumerate() {
            run(&mut list, command, index as i64 * 10, now);
        }
        let commands: Vec<_> = list.blocks().map(|block| block.command.as_str()).collect();
        assert_eq!(commands, ["b", "c"]);
    }

    #[test]
    fn clear_keeps_running_block() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        run(&mut list, "done", 0, now);
        list.apply(PromptStart, 10, now);
        list.apply(CommandStart, 10, now);
        list.apply(text("sleep 9"), 10, now);
        list.apply(OutputStart, 11, now);
        list.clear();
        assert_eq!(list.phase(), &Phase::Running);
        let blocks: Vec<_> = list.blocks().collect();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].command, "sleep 9");
        assert_eq!(blocks[0].output_line, Some(0));
        assert!(matches!(
            list.apply(CommandEnd { exit_code: Some(0) }, 2, now),
            Some(BlockEvent::Finished(_))
        ));
    }

    #[test]
    fn clear_at_prompt_keeps_command_start() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        run(&mut list, "done", 0, now);
        list.apply(PromptStart, 10, now);
        list.apply(CommandStart, 10, now);
        list.clear();
        assert_eq!(list.blocks().count(), 0);
        list.apply(text("ls"), 0, now);
        assert!(matches!(
            list.apply(OutputStart, 1, now),
            Some(BlockEvent::Started(_))
        ));
        assert_eq!(list.blocks().next().map(|block| block.prompt_line), Some(0));
    }

    #[test]
    fn command_text_after_output_start_names_the_block() {
        // Another integration (iTerm2, p10k) whose preexec runs before ours sends C first.
        let now = Instant::now();
        let mut list = BlockList::new(10);
        list.apply(PromptStart, 0, now);
        list.apply(CommandStart, 0, now);
        list.apply(OutputStart, 1, now);
        list.apply(
            CommandText {
                command: " secret".into(),
                save_to_history: false,
            },
            1,
            now,
        );
        list.apply(OutputStart, 1, now);
        // A later command's text must not rename a block that already has one.
        list.apply(text("other"), 1, now);
        let block = list.blocks().next().expect("block");
        assert_eq!(block.command, " secret");
        assert!(!block.save_to_history);
        assert_eq!(list.phase(), &Phase::Running);
    }

    #[test]
    fn branch_is_stamped_on_new_blocks() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        run(&mut list, "a", 0, now);
        list.set_branch(Some("main".into()));
        run(&mut list, "b", 10, now);
        let branches: Vec<_> = list.blocks().map(|block| block.branch.clone()).collect();
        assert_eq!(branches, [None, Some("main".to_string())]);
    }

    #[test]
    fn per_command_overhead_under_1ms() {
        const COMMANDS: u32 = 10_000;
        // What zsh sends for one command, as the terminal hands it over: raw OSC params.
        let cycle: Vec<Vec<Vec<u8>>> = [
            &["file://host/Users/me/project"][..],
            &["A"],
            &["B"],
            &["E", "git status \\x3b ls"],
            &["C"],
            &["D", "0"],
        ]
        .iter()
        .map(|fields| {
            fields
                .iter()
                .map(|field| field.as_bytes().to_vec())
                .collect()
        })
        .collect();
        let mut list = BlockList::new(1000);
        let started = Instant::now();
        for command in 0..COMMANDS {
            let line = i64::from(command) * 3;
            for params in &cycle {
                if let Some(mark) = crate::marks::parse_shell_mark(params) {
                    list.apply(mark, line, Instant::now());
                }
            }
        }
        let mean = started.elapsed() / COMMANDS;
        eprintln!("mean per-command mark handling: {mean:?}");
        assert_eq!(list.blocks().count(), 1000);
        assert!(mean < std::time::Duration::from_millis(1), "{mean:?}");
    }
}
