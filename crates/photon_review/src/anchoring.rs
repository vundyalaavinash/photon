use std::collections::HashMap;
use std::ops::RangeInclusive;

use crate::{Draft, PrKey, Side};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reanchor {
    Placed { start_line: u32, end_line: u32 },
    Outdated,
}

pub fn reanchor(anchor_text: &str, original_start: u32, new_text: Option<&str>) -> Reanchor {
    let Some(new_text) = new_text else {
        return Reanchor::Outdated;
    };
    // `split` keeps blank lines that `lines` would drop; a file's final newline ends its last
    // line rather than starting another one.
    let new_text = new_text.strip_suffix('\n').unwrap_or(new_text);
    let anchor: Vec<&str> = anchor_text.split('\n').map(str::trim_end).collect();
    let lines: Vec<&str> = new_text.split('\n').map(str::trim_end).collect();
    if anchor.len() > lines.len() {
        return Reanchor::Outdated;
    }
    let length = anchor.len() as u32;
    lines
        .windows(anchor.len())
        .enumerate()
        .filter(|(_, window)| *window == anchor.as_slice())
        .map(|(index, _)| index as u32 + 1)
        .min_by_key(|start| (start.abs_diff(original_start), *start))
        .map_or(Reanchor::Outdated, |start_line| Reanchor::Placed {
            start_line,
            end_line: start_line + length - 1,
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    Line {
        path: String,
        side: Side,
        start_line: Option<u32>,
        line: u32,
    },
    NearestLine {
        path: String,
        line: u32,
    },
    ReviewBody {
        path: String,
    },
}

pub fn place(draft: &Draft, commentable: &HashMap<String, Vec<RangeInclusive<u32>>>) -> Placement {
    let path = draft.path.clone();
    let ranges = commentable
        .get(&draft.path)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if draft.general || draft.outdated {
        return Placement::ReviewBody { path };
    }
    let line = |side| Placement::Line {
        path: draft.path.clone(),
        side,
        start_line: (draft.start_line != draft.end_line).then_some(draft.start_line),
        line: draft.end_line,
    };
    // Left rows are deleted base rows, which only exist inside the diff, even when the whole
    // file is deleted and has no commentable head lines.
    if draft.side == Side::Left {
        return line(Side::Left);
    }
    if ranges.is_empty() {
        return Placement::ReviewBody { path };
    }
    if ranges
        .iter()
        .any(|range| range.contains(&draft.start_line) && range.contains(&draft.end_line))
    {
        return line(Side::Right);
    }
    let nearest = ranges
        .iter()
        .map(|range| draft.start_line.clamp(*range.start(), *range.end()))
        .min_by_key(|line| (line.abs_diff(draft.start_line), *line))
        .unwrap_or(draft.start_line);
    Placement::NearestLine {
        path,
        line: nearest,
    }
}

fn encode_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub fn permalink(
    key: &PrKey,
    head_sha: &str,
    path: &str,
    start_line: u32,
    end_line: u32,
) -> String {
    let path = path
        .split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/");
    let lines = if start_line == end_line {
        format!("L{start_line}")
    } else {
        format!("L{start_line}-L{end_line}")
    };
    format!(
        "https://{}/{}/{}/blob/{head_sha}/{path}#{lines}",
        key.host, key.owner, key.repo
    )
}

pub fn quoted_body(draft: &Draft, key: &PrKey, head_sha: &str, language: Option<&str>) -> String {
    let lines = if draft.start_line == draft.end_line {
        format!("line {}", draft.start_line)
    } else {
        format!("lines {}–{}", draft.start_line, draft.end_line)
    };
    let link = permalink(key, head_sha, &draft.path, draft.start_line, draft.end_line);
    let fence = if draft.anchor_text.contains("```") {
        "~~~~"
    } else {
        "```"
    };
    format!(
        "Unchanged code, [{} {lines}]({link}):\n\n{fence}{}\n{}\n{fence}\n\n{}",
        draft.path,
        language.unwrap_or_default(),
        draft.anchor_text,
        draft.body
    )
}

pub fn review_body(summary: &str, body_drafts: &[(&Draft, String)]) -> String {
    let mut sorted: Vec<&(&Draft, String)> = body_drafts.iter().collect();
    sorted.sort_by(|(left, _), (right, _)| {
        (&left.path, left.start_line).cmp(&(&right.path, right.start_line))
    });
    let mut parts: Vec<String> = Vec::new();
    if !summary.is_empty() {
        parts.push(summary.to_string());
    }
    for group in sorted.chunk_by(|(left, _), (right, _)| left.path == right.path) {
        let Some((first, _)) = group.first() else {
            continue;
        };
        let bodies: Vec<&str> = group
            .iter()
            .map(|(_, body)| body.as_str())
            .filter(|body| !body.is_empty())
            .collect();
        if bodies.is_empty() {
            continue;
        }
        parts.push(format!("### {}\n\n{}", first.path, bodies.join("\n\n")));
    }
    parts.join("\n\n")
}

pub fn landing_note(placement: &Placement) -> Option<String> {
    match placement {
        Placement::Line { .. } => None,
        Placement::NearestLine { line, .. } => Some(format!(
            "Unchanged code: GitHub can't take this comment directly, so Photon posts it on line {line} and quotes these lines."
        )),
        Placement::ReviewBody { path } => Some(format!(
            "Unchanged file: GitHub can't take this comment directly, so Photon adds it to the review summary under {path}, with a quote."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PrKey {
        PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "web".into(),
            number: 482,
        }
    }

    fn draft(path: &str, side: Side, start_line: u32, end_line: u32) -> Draft {
        Draft {
            id: None,
            key: key(),
            path: path.into(),
            side,
            start_line,
            end_line,
            commit: "abc".into(),
            anchor_text: "let a = 1;".into(),
            body: "Why?".into(),
            outdated: false,
            general: false,
            post_state: crate::PostState::Unposted,
        }
    }

    fn numbered(lines: &[(u32, &str)], total: u32) -> String {
        (1..=total)
            .map(|line| {
                lines
                    .iter()
                    .find(|(number, _)| *number == line)
                    .map(|(_, text)| text.to_string())
                    .unwrap_or_else(|| format!("filler {line}"))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    const ANCHOR: &str = "fn verify() {\n    check();";

    #[test]
    fn reanchor_same_place() {
        let text = numbered(&[(3, "fn verify() {"), (4, "    check();")], 10);
        assert_eq!(
            reanchor(ANCHOR, 3, Some(&text)),
            Reanchor::Placed {
                start_line: 3,
                end_line: 4
            }
        );
    }

    #[test]
    fn reanchor_moved_lines() {
        let text = numbered(&[(6, "fn verify() {"), (7, "    check();")], 10);
        assert_eq!(
            reanchor(ANCHOR, 3, Some(&text)),
            Reanchor::Placed {
                start_line: 6,
                end_line: 7
            }
        );
    }

    #[test]
    fn reanchor_prefers_nearest_duplicate() {
        let text = numbered(
            &[
                (5, "fn verify() {"),
                (6, "    check();"),
                (40, "fn verify() {"),
                (41, "    check();"),
            ],
            50,
        );
        assert_eq!(
            reanchor(ANCHOR, 38, Some(&text)),
            Reanchor::Placed {
                start_line: 40,
                end_line: 41
            }
        );
        // Equally far from both: the earlier one wins.
        let text = numbered(
            &[
                (10, "fn verify() {"),
                (11, "    check();"),
                (20, "fn verify() {"),
                (21, "    check();"),
            ],
            30,
        );
        assert_eq!(
            reanchor(ANCHOR, 15, Some(&text)),
            Reanchor::Placed {
                start_line: 10,
                end_line: 11
            }
        );
    }

    #[test]
    fn reanchor_text_gone_is_outdated() {
        let text = numbered(&[(3, "fn verify() {"), (4, "    check_all();")], 10);
        assert_eq!(reanchor(ANCHOR, 3, Some(&text)), Reanchor::Outdated);
    }

    #[test]
    fn reanchor_single_blank_line_unmoved() {
        assert_eq!(
            reanchor("", 3, Some("a\nb\n\nc\n")),
            Reanchor::Placed {
                start_line: 3,
                end_line: 3
            }
        );
        assert_eq!(reanchor("", 2, Some("a\nb\n")), Reanchor::Outdated);
    }

    #[test]
    fn reanchor_range_ending_in_blank_line_keeps_length() {
        assert_eq!(
            reanchor("fn a() {\n", 2, Some("x\nfn a() {\n  \r\ny\n")),
            Reanchor::Placed {
                start_line: 2,
                end_line: 3
            }
        );
    }

    #[test]
    fn reanchor_deleted_file_is_outdated() {
        assert_eq!(reanchor(ANCHOR, 3, None), Reanchor::Outdated);
    }

    #[test]
    fn reanchor_ignores_trailing_whitespace_and_crlf() {
        let text = "first\r\nfn verify() {  \r\n    check();\t\r\nlast\r\n";
        assert_eq!(
            reanchor("fn verify() {\n    check();  ", 1, Some(text)),
            Reanchor::Placed {
                start_line: 2,
                end_line: 3
            }
        );
    }

    fn commentable(
        path: &str,
        ranges: Vec<RangeInclusive<u32>>,
    ) -> HashMap<String, Vec<RangeInclusive<u32>>> {
        HashMap::from_iter([(path.to_string(), ranges)])
    }

    #[test]
    fn place_on_diff_line() {
        let ranges = commentable("src/a.rs", vec![10..=16, 30..=31]);
        assert_eq!(
            place(&draft("src/a.rs", Side::Right, 12, 12), &ranges),
            Placement::Line {
                path: "src/a.rs".into(),
                side: Side::Right,
                start_line: None,
                line: 12
            }
        );
        assert_eq!(
            place(&draft("src/a.rs", Side::Right, 12, 14), &ranges),
            Placement::Line {
                path: "src/a.rs".into(),
                side: Side::Right,
                start_line: Some(12),
                line: 14
            }
        );
        assert_eq!(
            place(&draft("src/a.rs", Side::Left, 3, 4), &ranges),
            Placement::Line {
                path: "src/a.rs".into(),
                side: Side::Left,
                start_line: Some(3),
                line: 4
            }
        );
    }

    #[test]
    fn place_left_draft_on_deleted_file() {
        let ranges = commentable("src/gone.rs", Vec::new());
        assert_eq!(
            place(&draft("src/gone.rs", Side::Left, 4, 4), &ranges),
            Placement::Line {
                path: "src/gone.rs".into(),
                side: Side::Left,
                start_line: None,
                line: 4
            }
        );
    }

    #[test]
    fn place_range_spanning_hunk_edge_uses_nearest() {
        let ranges = commentable("src/a.rs", vec![10..=16, 30..=31]);
        assert_eq!(
            place(&draft("src/a.rs", Side::Right, 15, 18), &ranges),
            Placement::NearestLine {
                path: "src/a.rs".into(),
                line: 15
            }
        );
        assert_eq!(
            place(&draft("src/a.rs", Side::Right, 8, 11), &ranges),
            Placement::NearestLine {
                path: "src/a.rs".into(),
                line: 10
            }
        );
    }

    #[test]
    fn place_unchanged_line_in_changed_file_uses_nearest_diff_line() {
        let ranges = commentable("src/a.rs", vec![10..=16, 30..=31]);
        assert_eq!(
            place(&draft("src/a.rs", Side::Right, 27, 28), &ranges),
            Placement::NearestLine {
                path: "src/a.rs".into(),
                line: 30
            }
        );
        assert_eq!(
            place(&draft("src/a.rs", Side::Right, 2, 2), &ranges),
            Placement::NearestLine {
                path: "src/a.rs".into(),
                line: 10
            }
        );
        // 23 is 7 lines from both 16 and 30: the earlier line wins.
        assert_eq!(
            place(&draft("src/a.rs", Side::Right, 23, 23), &ranges),
            Placement::NearestLine {
                path: "src/a.rs".into(),
                line: 16
            }
        );
    }

    #[test]
    fn place_unchanged_file_goes_to_body() {
        let ranges = commentable("src/a.rs", vec![10..=16]);
        assert_eq!(
            place(&draft("src/b.rs", Side::Right, 3, 3), &ranges),
            Placement::ReviewBody {
                path: "src/b.rs".into()
            }
        );
        let empty = commentable("src/b.rs", Vec::new());
        assert_eq!(
            place(&draft("src/b.rs", Side::Right, 3, 3), &empty),
            Placement::ReviewBody {
                path: "src/b.rs".into()
            }
        );
    }

    #[test]
    fn place_general_and_outdated_go_to_body() {
        let ranges = commentable("src/a.rs", vec![10..=16]);
        let mut general = draft("src/a.rs", Side::Right, 12, 12);
        general.general = true;
        let mut outdated = draft("src/a.rs", Side::Right, 12, 12);
        outdated.outdated = true;
        for draft in [general, outdated] {
            assert_eq!(
                place(&draft, &ranges),
                Placement::ReviewBody {
                    path: "src/a.rs".into()
                }
            );
        }
    }

    #[test]
    fn quoted_body_exact() {
        let mut single = draft("src/a.rs", Side::Right, 12, 12);
        single.anchor_text = "    check();".into();
        assert_eq!(
            quoted_body(&single, &key(), "sha1", Some("rust")),
            "Unchanged code, [src/a.rs line 12](https://github.com/acme/web/blob/sha1/src/a.rs#L12):\n\
             \n\
             ```rust\n    check();\n```\n\
             \n\
             Why?"
        );

        let mut fenced = draft("docs/a.md", Side::Right, 3, 5);
        fenced.anchor_text = "```sh\nmake\n```".into();
        assert_eq!(
            quoted_body(&fenced, &key(), "sha1", None),
            "Unchanged code, [docs/a.md lines 3–5](https://github.com/acme/web/blob/sha1/docs/a.md#L3-L5):\n\
             \n\
             ~~~~\n```sh\nmake\n```\n~~~~\n\
             \n\
             Why?"
        );
    }

    #[test]
    fn review_body_groups_by_file() {
        let b_late = draft("src/b.rs", Side::Right, 20, 20);
        let a = draft("src/a.rs", Side::Right, 7, 7);
        let b_early = draft("src/b.rs", Side::Right, 3, 4);
        let body = review_body(
            "Looks good overall.",
            &[
                (&b_late, "B20".to_string()),
                (&a, "A7".to_string()),
                (&b_early, "B3".to_string()),
            ],
        );
        assert_eq!(
            body,
            "Looks good overall.\n\n### src/a.rs\n\nA7\n\n### src/b.rs\n\nB3\n\nB20"
        );
        assert_eq!(
            review_body("", &[(&a, "A7".to_string())]),
            "### src/a.rs\n\nA7"
        );
        assert_eq!(review_body("Only this.", &[]), "Only this.");
    }

    #[test]
    fn permalink_encodes_path() {
        assert_eq!(
            permalink(&key(), "abc123", "src/a b.rs", 12, 14),
            "https://github.com/acme/web/blob/abc123/src/a%20b.rs#L12-L14"
        );
        assert_eq!(
            permalink(&key(), "abc123", "src/é#.rs", 3, 3),
            "https://github.com/acme/web/blob/abc123/src/%C3%A9%23.rs#L3"
        );
    }

    #[test]
    fn landing_notes() {
        assert_eq!(
            landing_note(&Placement::NearestLine {
                path: "src/a.rs".into(),
                line: 43
            })
            .as_deref(),
            Some(
                "Unchanged code: GitHub can't take this comment directly, so Photon posts it on line 43 and quotes these lines."
            )
        );
        assert_eq!(
            landing_note(&Placement::ReviewBody {
                path: "src/b.rs".into()
            })
            .as_deref(),
            Some(
                "Unchanged file: GitHub can't take this comment directly, so Photon adds it to the review summary under src/b.rs, with a quote."
            )
        );
        assert_eq!(
            landing_note(&Placement::Line {
                path: "src/a.rs".into(),
                side: Side::Right,
                start_line: None,
                line: 3
            }),
            None
        );
    }
}
