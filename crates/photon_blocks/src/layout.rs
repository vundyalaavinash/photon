use crate::block_list::{BlockId, BlockList};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleBlock {
    pub id: BlockId,
    pub first_row: i32,
    pub last_row: i32,
    pub header_row: Option<i32>,
}

pub fn visible_blocks(
    list: &BlockList,
    top_absolute_line: i64,
    screen_rows: i32,
) -> Vec<VisibleBlock> {
    let bottom_row = i64::from(screen_rows) - 1;
    let to_row = |value: i64| value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    list.blocks()
        .filter_map(|block| {
            let first = block.prompt_line - top_absolute_line;
            // A running block has no end yet, so it extends to the screen bottom; a finished
            // block's end line is exclusive, so ending on row 0 leaves nothing on screen.
            let last = block
                .end_line
                .map_or(bottom_row, |end| end - top_absolute_line);
            let past_top = if block.end_line.is_some() {
                last <= 0
            } else {
                last < 0
            };
            if past_top || first > bottom_row {
                return None;
            }
            Some(VisibleBlock {
                id: block.id,
                first_row: to_row(first),
                last_row: to_row(last),
                header_row: (first < 0).then_some(0),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockList, ShellMark::*};
    use std::time::Instant;

    #[test]
    fn visible_blocks_maps_rows() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        list.apply(PromptStart, 100, now);
        list.apply(CommandStart, 100, now);
        list.apply(OutputStart, 101, now);
        list.apply(CommandEnd { exit_code: Some(0) }, 140, now);
        let visible = visible_blocks(&list, 120, 30);
        assert_eq!(visible.len(), 1);
        assert_eq!((visible[0].first_row, visible[0].last_row), (-20, 20));
        assert_eq!(visible[0].header_row, Some(0));
        assert!(visible_blocks(&list, 200, 30).is_empty());
    }

    #[test]
    fn finished_block_ending_at_top_is_not_visible() {
        let now = Instant::now();
        let mut list = BlockList::new(10);
        list.apply(PromptStart, 100, now);
        list.apply(CommandStart, 100, now);
        list.apply(OutputStart, 101, now);
        list.apply(CommandEnd { exit_code: Some(0) }, 140, now);
        // `end_line` is exclusive: line 140 belongs to whatever comes next.
        assert!(visible_blocks(&list, 140, 30).is_empty());
        assert_eq!(visible_blocks(&list, 139, 30).len(), 1);
    }
}
