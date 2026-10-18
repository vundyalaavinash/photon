// photon: tests for shell-integration OSC marks (133/633/7) and absolute line numbering.
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi;

type Mark = (Vec<Vec<u8>>, i64);

#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<Mark>>>);

impl EventListener for Collector {
    fn send_event(&self, event: Event) {
        if let Event::ShellIntegration { params, absolute_line, .. } = event {
            self.0.lock().expect("collector lock").push((params, absolute_line));
        }
    }
}

fn feed_with_resize(history: usize, lines: usize, chunks: &[&str], resize_to: usize) -> Vec<Mark> {
    let collector = Collector::default();
    let config = Config { scrolling_history: history, ..Config::default() };
    let mut term = Term::new(config, &TermSize::new(80, lines), collector.clone());
    let mut parser: ansi::Processor = ansi::Processor::new();
    for chunk in chunks {
        parser.advance(&mut term, chunk.as_bytes());
    }
    term.resize(TermSize::new(80, resize_to));
    term.resize(TermSize::new(80, lines));
    parser.advance(&mut term, b"\x1b]133;B\x07");
    let marks = collector.0.lock().expect("collector lock").clone();
    marks
}

fn feed(history: usize, lines: usize, chunks: &[&str]) -> Vec<Mark> {
    let collector = Collector::default();
    let config = Config { scrolling_history: history, ..Config::default() };
    let mut term = Term::new(config, &TermSize::new(80, lines), collector.clone());
    let mut parser: ansi::Processor = ansi::Processor::new();
    for chunk in chunks {
        parser.advance(&mut term, chunk.as_bytes());
    }
    let marks = collector.0.lock().expect("collector lock").clone();
    marks
}

#[test]
fn osc_133_is_forwarded_with_line() {
    let marks = feed(100, 10, &["a\r\nb\r\n\x1b]133;A\x07"]);
    assert_eq!(marks, vec![(vec![b"A".to_vec()], 2)]);
}

#[test]
fn osc_7_and_633_are_forwarded() {
    let marks = feed(100, 10, &["\x1b]7;file://h/tmp\x07\x1b]633;E;ls -la\x07"]);
    assert_eq!(
        marks,
        vec![
            (vec![b"file://h/tmp".to_vec()], 0),
            (vec![b"E".to_vec(), b"ls -la".to_vec()], 0),
        ]
    );
}

#[test]
fn absolute_lines_survive_history_eviction() {
    let thousand = "x\r\n".repeat(1000);
    let five = "y\r\n".repeat(5);
    let marks =
        feed(100, 10, &[&thousand, "\x1b]133;A\x07", &five, "\x1b]133;A\x07"]);
    let lines: Vec<i64> = marks.iter().map(|mark| mark.1).collect();
    assert_eq!(lines, vec![1000, 1005]);
}

#[test]
fn other_osc_still_unhandled() {
    assert!(feed(100, 10, &["\x1b]1337;File=x\x07"]).is_empty());
}

#[test]
fn absolute_line_is_stable_across_row_shrink_and_grow() {
    let thirty = "x\r\n".repeat(30);
    let marks = feed_with_resize(100, 10, &[&thirty, "\x1b]133;A\x07"], 5);
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[0].1, marks[1].1);
}

#[test]
fn absolute_line_is_stable_when_growing_without_history() {
    let marks = feed_with_resize(100, 10, &["a\r\nb\r\n\x1b]133;A\x07"], 13);
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[0].1, marks[1].1);
}
