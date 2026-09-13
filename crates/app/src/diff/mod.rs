mod gaps;
mod highlight;
mod render;

pub(crate) use gaps::{push_gap_rows, run_upgrade, FileUpgrade, UpgradeJob, UpgradeSource};
pub(crate) use highlight::{hunk_syntax, merge_highlights, MAX_SOURCE_HIGHLIGHT_BYTES, MAX_SYNTAX_LINE_BYTES};

use crate::comments::{now_unix, push_thread_rows, CommentIndex, CommentSide};
use crate::theme;
use diff_core::{DiffRow, FileStatus, PrDiff};
use gpui::{div, prelude::*, SharedString, StyledText};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU32, Ordering};
/// Diff pane font size in px, adjustable at runtime (cmd-+ / cmd-- / cmd-0).
/// A process-wide cell rather than a plumbed parameter: it feeds free
/// functions (row building, minimap, hit tests) as well as render, and the app
/// is a single window, so threading it through every call site would be pure
/// churn. Only the main thread writes it.
pub(crate) static FONT_PX: AtomicU32 = AtomicU32::new(DEFAULT_TEXT_SIZE as u32);
pub(crate) const DEFAULT_TEXT_SIZE: f32 = 13.0;
/// Zoom bounds; below ~7px glyphs stop being legible, above ~28px a diff row
/// wastes most of the window.
pub(crate) const MIN_TEXT_SIZE: f32 = 7.0;
pub(crate) const MAX_TEXT_SIZE: f32 = 28.0;
/// Row height as a multiple of the font size. 13 * 1.7 ≈ 22, the height this
/// pane used before zoom existed, so the default look is unchanged.
const LINE_HEIGHT_RATIO: f32 = 1.7;

pub(crate) fn text_size() -> f32 {
    FONT_PX.load(Ordering::Relaxed) as f32
}

/// Row height for a given font size. Kept pure so it's testable without
/// touching the process-wide size.
pub(crate) fn row_height_for(size: f32) -> f32 {
    (size * LINE_HEIGHT_RATIO).round()
}

/// Height of one diff row. Derived from the font so text never outgrows its
/// row; every scroll/hit-test/minimap calculation keys off this.
pub(crate) fn row_height() -> f32 {
    row_height_for(text_size())
}

/// Gutter widths in px, matching render_row's fixed-width children: unified is
/// two 44px line-number columns + a 28px marker; each split cell is one of
/// each. Mouse→column math depends on these.
pub(crate) const UNIFIED_GUTTER: f32 = 44. + 44. + 28.;
pub(crate) const SPLIT_GUTTER: f32 = 44. + 28.;
pub(crate) const SPLIT_DIVIDER: f32 = 6.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewMode {
    Unified,
    Split,
}

/// One side of a split row: line number, kind, text, word-level highlights,
/// and tree-sitter token spans.
pub(crate) struct Cell {
    pub(crate) no: u32,
    pub(crate) kind: LineKind,
    pub(crate) text: SharedString,
    pub(crate) intra: Vec<Range<usize>>,
    pub(crate) syntax: Vec<(Range<usize>, syntax::Token)>,
}

pub(crate) enum Row {
    Spacer,
    FileHeader {
        path: SharedString,
        old_path: Option<SharedString>,
        status: FileStatus,
        additions: u32,
        deletions: u32,
        /// Review comments anchored in this file / outdated (unanchorable)
        /// ones. Shown as a dim count when nonzero; always populated for PR
        /// items even while comment rows are toggled off.
        comments: usize,
        outdated: usize,
    },
    HunkHeader {
        label: SharedString,
        /// True once the file's hunks come from the Phase-2 full-content
        /// re-diff (renders the label in a subtly different color).
        upgraded: bool,
    },
    Binary,
    /// Hidden shared lines in an upgraded file: before the first hunk, between
    /// hunks, or after the last one. Clicking expands the whole gap.
    /// Selectable-through like headers (`row_side_text` returns None).
    Gap {
        file_ix: usize,
        gap_ix: usize,
        hidden: u32,
    },
    Line {
        old_no: Option<u32>,
        new_no: Option<u32>,
        kind: LineKind,
        text: SharedString,
        intra: Vec<Range<usize>>,
        syntax: Vec<(Range<usize>, syntax::Token)>,
    },
    SplitLine {
        left: Option<Cell>,
        right: Option<Cell>,
    },
    /// First row of one review comment: author + age. Selectable-through
    /// like headers. In split mode `half` places the card in the pane the
    /// comment was left on (LEFT/RIGHT, like GitHub); None spans full width.
    CommentHeader {
        author: SharedString,
        when: SharedString,
        is_reply: bool,
        half: Option<CommentSide>,
        /// First row of the whole thread, so it draws the card's top edge.
        top: bool,
    },
    /// One soft-wrapped line of a comment body (wrapped at
    /// [`COMMENT_WRAP_CHARS`] when the rows are built).
    CommentBody {
        line: SharedString,
        half: Option<CommentSide>,
    },
    /// The "↳ reply" affordance closing a thread; clicking opens the
    /// composer targeting `post_reply` on the thread's root comment.
    CommentActions {
        root_id: u64,
        path: SharedString,
        side: CommentSide,
        line: u64,
        half: Option<CommentSide>,
    },
}

pub(crate) fn is_comment_row(row: &Row) -> bool {
    matches!(
        row,
        Row::CommentHeader { .. } | Row::CommentBody { .. } | Row::CommentActions { .. }
    )
}

/// Index of the `n`-th non-comment row (0-based), clamped to the last row.
/// Comment rows are pure insertions relative to the diff rows, so this maps
/// a position across rebuilds that only add or remove comment rows.
pub(crate) fn nth_noncomment_row(rows: &[Row], n: usize) -> usize {
    let mut seen = 0;
    for (ix, row) in rows.iter().enumerate() {
        if !is_comment_row(row) {
            if seen == n {
                return ix;
            }
            seen += 1;
        }
    }
    rows.len().saturating_sub(1)
}

/// Flatten the diff into display rows plus the row indices of file headers and
/// hunk headers. Split mode pairs removed/added runs positionally into
/// two-cell rows; unequal runs leave one-sided rows. Files present in
/// `upgrades` use whole-file syntax spans and get gap rows between hunks.
/// Review threads render beneath the line they anchor to when
/// `show_comments` is set; file headers always carry the comment counts.
pub(crate) fn build_rows(
    diff: &PrDiff,
    mode: ViewMode,
    upgrades: &HashMap<usize, FileUpgrade>,
    comments: Option<&CommentIndex>,
    show_comments: bool,
    wrap: usize,
) -> (Vec<Row>, Vec<usize>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut file_rows = Vec::new();
    let mut hunk_rows = Vec::new();
    let now = now_unix();

    for (file_ix, file) in diff.files.iter().enumerate() {
        let upgrade = upgrades.get(&file_ix);
        let path = file.display_path();
        let (n_comments, n_outdated) = comments
            .and_then(|index| index.counts.get(path))
            .copied()
            .unwrap_or((0, 0));
        let anchors = if show_comments {
            comments.and_then(|index| index.threads.get(path))
        } else {
            None
        };
        if !rows.is_empty() {
            rows.push(Row::Spacer);
        }
        file_rows.push(rows.len());
        rows.push(Row::FileHeader {
            path: path.to_string().into(),
            old_path: match file.status {
                FileStatus::Renamed => file.old_path.clone().map(Into::into),
                _ => None,
            },
            status: file.status,
            additions: file.additions,
            deletions: file.deletions,
            comments: n_comments,
            outdated: n_outdated,
        });
        if file.status == FileStatus::Binary {
            rows.push(Row::Binary);
            continue;
        }
        let lang = syntax::language_for_path(path);
        for (hunk_ix, hunk) in file.hunks.iter().enumerate() {
            if let Some(upgrade) = upgrade {
                push_gap_rows(
                    &mut rows,
                    upgrade,
                    &file.hunks,
                    file_ix,
                    hunk_ix,
                    mode,
                    path,
                    anchors,
                    now,
                    wrap,
                );
            }
            let syntax_spans = match upgrade {
                Some(upgrade) => hunk.rows.iter().map(|row| upgrade.row_spans(row)).collect(),
                None => hunk_syntax(lang, &hunk.rows),
            };
            hunk_rows.push(rows.len());
            let mut label = format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            );
            if !hunk.section.is_empty() {
                label.push(' ');
                label.push_str(&hunk.section);
            }
            rows.push(Row::HunkHeader {
                label: label.into(),
                upgraded: upgrade.is_some(),
            });
            match mode {
                ViewMode::Unified => {
                    for (ix, row) in hunk.rows.iter().enumerate() {
                        let syntax = syntax_spans[ix].clone();
                        let (old_no, new_no) = match row {
                            DiffRow::Context { old_no, new_no, .. } => {
                                (Some(*old_no), Some(*new_no))
                            }
                            DiffRow::Added { new_no, .. } => (None, Some(*new_no)),
                            DiffRow::Removed { old_no, .. } => (Some(*old_no), None),
                        };
                        rows.push(match row {
                            DiffRow::Context {
                                old_no,
                                new_no,
                                text,
                            } => Row::Line {
                                old_no: Some(*old_no),
                                new_no: Some(*new_no),
                                kind: LineKind::Context,
                                text: text.clone().into(),
                                intra: Vec::new(),
                                syntax,
                            },
                            DiffRow::Added {
                                new_no,
                                text,
                                intra,
                            } => Row::Line {
                                old_no: None,
                                new_no: Some(*new_no),
                                kind: LineKind::Added,
                                text: text.clone().into(),
                                intra: intra.clone(),
                                syntax,
                            },
                            DiffRow::Removed {
                                old_no,
                                text,
                                intra,
                            } => Row::Line {
                                old_no: Some(*old_no),
                                new_no: None,
                                kind: LineKind::Removed,
                                text: text.clone().into(),
                                intra: intra.clone(),
                                syntax,
                            },
                        });
                        push_thread_rows(&mut rows, anchors, path, CommentSide::Left, old_no, now, ViewMode::Unified, wrap);
                        push_thread_rows(&mut rows, anchors, path, CommentSide::Right, new_no, now, ViewMode::Unified, wrap);
                    }
                }
                ViewMode::Split => {
                    // Same run-scan shape as diff-core's compute_intra_line:
                    // a run of Removed immediately followed by a run of Added
                    // pairs positionally; the excess (and lone runs) render
                    // one-sided.
                    let hrows = &hunk.rows;
                    let mut i = 0;
                    while i < hrows.len() {
                        match &hrows[i] {
                            DiffRow::Context {
                                old_no,
                                new_no,
                                text,
                            } => {
                                let text: SharedString = text.clone().into();
                                let syntax = syntax_spans[i].clone();
                                rows.push(Row::SplitLine {
                                    left: Some(Cell {
                                        no: *old_no,
                                        kind: LineKind::Context,
                                        text: text.clone(),
                                        intra: Vec::new(),
                                        syntax: syntax.clone(),
                                    }),
                                    right: Some(Cell {
                                        no: *new_no,
                                        kind: LineKind::Context,
                                        text,
                                        intra: Vec::new(),
                                        syntax,
                                    }),
                                });
                                push_thread_rows(
                                    &mut rows,
                                    anchors,
                                    path,
                                    CommentSide::Left,
                                    Some(*old_no),
                                    now,
                                    ViewMode::Split, wrap,
                                );
                                push_thread_rows(
                                    &mut rows,
                                    anchors,
                                    path,
                                    CommentSide::Right,
                                    Some(*new_no),
                                    now,
                                    ViewMode::Split, wrap,
                                );
                                i += 1;
                            }
                            DiffRow::Added {
                                new_no,
                                text,
                                intra,
                            } => {
                                // Added run with no preceding Removed run.
                                rows.push(Row::SplitLine {
                                    left: None,
                                    right: Some(Cell {
                                        no: *new_no,
                                        kind: LineKind::Added,
                                        text: text.clone().into(),
                                        intra: intra.clone(),
                                        syntax: syntax_spans[i].clone(),
                                    }),
                                });
                                push_thread_rows(
                                    &mut rows,
                                    anchors,
                                    path,
                                    CommentSide::Right,
                                    Some(*new_no),
                                    now,
                                    ViewMode::Split, wrap,
                                );
                                i += 1;
                            }
                            DiffRow::Removed { .. } => {
                                let start = i;
                                while i < hrows.len() && matches!(hrows[i], DiffRow::Removed { .. })
                                {
                                    i += 1;
                                }
                                let mid = i;
                                while i < hrows.len() && matches!(hrows[i], DiffRow::Added { .. }) {
                                    i += 1;
                                }
                                let (removed, added) = (mid - start, i - mid);
                                for pair in 0..removed.max(added) {
                                    let left =
                                        (pair < removed).then(|| match &hrows[start + pair] {
                                            DiffRow::Removed {
                                                old_no,
                                                text,
                                                intra,
                                            } => Cell {
                                                no: *old_no,
                                                kind: LineKind::Removed,
                                                text: text.clone().into(),
                                                intra: intra.clone(),
                                                syntax: syntax_spans[start + pair].clone(),
                                            },
                                            _ => unreachable!(),
                                        });
                                    let right = (pair < added).then(|| match &hrows[mid + pair] {
                                        DiffRow::Added {
                                            new_no,
                                            text,
                                            intra,
                                        } => Cell {
                                            no: *new_no,
                                            kind: LineKind::Added,
                                            text: text.clone().into(),
                                            intra: intra.clone(),
                                            syntax: syntax_spans[mid + pair].clone(),
                                        },
                                        _ => unreachable!(),
                                    });
                                    let left_no = left.as_ref().map(|cell| cell.no);
                                    let right_no = right.as_ref().map(|cell| cell.no);
                                    rows.push(Row::SplitLine { left, right });
                                    push_thread_rows(
                                        &mut rows,
                                        anchors,
                                        path,
                                        CommentSide::Left,
                                        left_no,
                                        now,
                                        ViewMode::Split, wrap,
                                    );
                                    push_thread_rows(
                                        &mut rows,
                                        anchors,
                                        path,
                                        CommentSide::Right,
                                        right_no,
                                        now,
                                        ViewMode::Split, wrap,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(upgrade) = upgrade {
            push_gap_rows(
                &mut rows,
                upgrade,
                &file.hunks,
                file_ix,
                file.hunks.len(),
                mode,
                path,
                anchors,
                now,
                wrap,
            );
        }
    }

    (rows, file_rows, hunk_rows)
}

/// Per-kind row tint, word-highlight tint, gutter marker, and marker color.
pub(crate) fn kind_style(
    kind: LineKind,
) -> (
    Option<gpui::Rgba>,
    Option<gpui::Rgba>,
    &'static str,
    gpui::Rgba,
) {
    match kind {
        LineKind::Context => (None, None, "", theme::overlay0()),
        LineKind::Added => (
            Some(theme::added_row_bg()),
            Some(theme::added_word_bg()),
            "+",
            theme::green(),
        ),
        LineKind::Removed => (
            Some(theme::removed_row_bg()),
            Some(theme::removed_word_bg()),
            "−",
            theme::red(),
        ),
    }
}

/// Line text with syntax colors overlaid with word-level highlight ranges and
/// the selection background, shared by unified rows and split cells.
pub(crate) fn line_content(
    text: &SharedString,
    syntax: &[(Range<usize>, syntax::Token)],
    intra: &[Range<usize>],
    word_bg: Option<gpui::Rgba>,
    selection: Option<Range<usize>>,
) -> gpui::AnyElement {
    let highlights = merge_highlights(syntax, intra, word_bg, selection);
    if highlights.is_empty() {
        div().child(text.clone()).into_any_element()
    } else {
        StyledText::new(text.clone())
            .with_highlights(highlights)
            .into_any_element()
    }
}

/// Shared shape of every comment row: indented card with a mantle background
/// and a blue left accent, spanning full width in both view modes.
/// Where a row sits in its thread's card, so the rows together draw one
/// continuous outline instead of a box per row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardEdge {
    Top,
    Middle,
    Bottom,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::{comment_anchor, group_comments, CommentSide, COMMENT_WRAP_CHARS};
    use crate::selection::{row_side_text, SelSide};
    use crate::test_util::{cell, mrow, rc, row_name, sample_diff};
    use crate::minimap::{minimap_rows, MinimapKind};
    use std::collections::HashMap;

    #[test]
    fn split_context_fills_both_cells() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        // rows[0] = FileHeader, rows[1] = HunkHeader, rows[2] = first context.
        match &rows[2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (1, LineKind::Context, "ctx", &[][..]));
                assert_eq!(cell(right), (1, LineKind::Context, "ctx", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_pairs_equal_runs_positionally() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        match &rows[3] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (2, LineKind::Removed, "old1", &[0..3][..]));
                assert_eq!(cell(right), (2, LineKind::Added, "new1", &[0..3][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[4] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (3, LineKind::Removed, "old2", &[][..]));
                assert_eq!(cell(right), (3, LineKind::Added, "new2", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        // Equal run + 2 context rows: 4 split lines for a 6-row hunk.
        match &rows[5] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left).2, "tail");
                assert_eq!(cell(right).2, "tail");
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_unequal_and_lone_runs_are_one_sided() {
        let (rows, _, hunk_rows) =
            build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let h2 = hunk_rows[1];
        // 2 removed / 1 added: first row paired, second left-only.
        match &rows[h2 + 1] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (10, LineKind::Removed, "r1", &[][..]));
                assert_eq!(cell(right), (10, LineKind::Added, "a1", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[h2 + 2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (11, LineKind::Removed, "r2", &[][..]));
                assert!(right.is_none());
            }
            _ => panic!("expected split line"),
        }
        // Lone added run after context: right-only.
        match &rows[h2 + 4] {
            Row::SplitLine { left, right } => {
                assert!(left.is_none());
                assert_eq!(cell(right), (12, LineKind::Added, "lone", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn header_indices_are_correct_in_both_modes() {
        let diff = sample_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, file_rows, hunk_rows) = build_rows(&diff, mode, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
            assert_eq!(file_rows.len(), 2);
            assert_eq!(hunk_rows.len(), 2);
            for &ix in &file_rows {
                assert!(matches!(rows[ix], Row::FileHeader { .. }));
            }
            for &ix in &hunk_rows {
                assert!(matches!(rows[ix], Row::HunkHeader { .. }));
            }
            // Binary file: header immediately followed by the binary row.
            assert!(matches!(rows[file_rows[1] + 1], Row::Binary));
        }
        // Unified emits one row per diff row; split collapses the equal run.
        let (unified, _, _) = build_rows(&diff, ViewMode::Unified, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let (split, _, _) = build_rows(&diff, ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let unified_lines = unified
            .iter()
            .filter(|r| matches!(r, Row::Line { .. }))
            .count();
        let split_lines = split
            .iter()
            .filter(|r| matches!(r, Row::SplitLine { .. }))
            .count();
        assert_eq!(unified_lines, 11);
        assert_eq!(split_lines, 8); // 4 (hunk 1) + 4 (hunk 2)
    }

    /// Comments for sample_diff's a.rs: a RIGHT thread (with one reply) on
    /// added line 2 ("new1"), a LEFT thread on removed line 2 ("old1"), and
    /// one outdated comment.
    fn sample_comments() -> CommentIndex {
        group_comments(vec![
            rc(
                1,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "on new1",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            rc(
                2,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "reply",
                "bob",
                "2026-01-02T00:00:00Z",
                Some(1),
            ),
            rc(
                3,
                "a.rs",
                Some("LEFT"),
                Some(2),
                "on old1",
                "carol",
                "2026-01-03T00:00:00Z",
                None,
            ),
            rc(
                4,
                "a.rs",
                None,
                None,
                "outdated",
                "dave",
                "2026-01-04T00:00:00Z",
                None,
            ),
        ])
    }

    #[test]
    fn unified_rows_insert_threads_beneath_their_anchor() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // rows: FileHeader, HunkHeader, ctx, rem old1 (old_no 2) + LEFT
        // thread, rem old2, add new1 (new_no 2) + RIGHT thread, …
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        assert_eq!(
            &names[..12],
            &[
                "FileHeader",
                "HunkHeader",
                "Line",          // ctx 1/1
                "Line",          // rem old1 (old 2)
                "CommentHeader", // carol on old1
                "CommentBody",
                "CommentActions",
                "Line",          // rem old2
                "Line",          // add new1 (new 2)
                "CommentHeader", // alice
                "CommentBody",
                "CommentHeader", // bob's reply
            ]
        );
        assert_eq!(names[12], "CommentBody");
        assert_eq!(names[13], "CommentActions");
        // The LEFT thread is carol's; the reply flag follows position.
        match &rows[4] {
            Row::CommentHeader {
                author, is_reply, ..
            } => {
                assert_eq!(author.as_ref(), "carol");
                assert!(!is_reply);
            }
            other => panic!("expected comment header, got {}", row_name(other)),
        }
        match &rows[11] {
            Row::CommentHeader {
                author, is_reply, ..
            } => {
                assert_eq!(author.as_ref(), "bob");
                assert!(is_reply);
            }
            other => panic!("expected reply header, got {}", row_name(other)),
        }
        // Actions row targets the thread root and carries the anchor.
        match &rows[13] {
            Row::CommentActions {
                root_id,
                path,
                side,
                line,
                ..
            } => {
                assert_eq!(*root_id, 1);
                assert_eq!(path.as_ref(), "a.rs");
                assert_eq!((*side, *line), (CommentSide::Right, 2));
            }
            other => panic!("expected actions row, got {}", row_name(other)),
        }
        // File header carries the counts (3 anchored, 1 outdated).
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (3, 1));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
        // Comment rows are selectable-through and blank in the minimap.
        assert!(row_side_text(&rows[4], SelSide::Unified).is_none());
        assert_eq!(minimap_rows(&rows)[4], mrow(MinimapKind::Blank, 0.));
    }

    #[test]
    fn split_rows_anchor_threads_by_cell() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // rows[3] pairs old1/new1 (both line 2): LEFT thread then RIGHT
        // thread directly beneath it.
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        assert_eq!(
            &names[2..11],
            &[
                "SplitLine",     // ctx
                "SplitLine",     // old1 | new1
                "CommentHeader", // carol (LEFT)
                "CommentBody",
                "CommentActions",
                "CommentHeader", // alice (RIGHT)
                "CommentBody",
                "CommentHeader", // bob reply
                "CommentBody",
            ]
        );
        assert_eq!(names[11], "CommentActions");
        assert_eq!(names[12], "SplitLine"); // old2 | new2
    }

    /// The half a comment row renders in, or None for a full-width card.
    fn row_half(row: &Row) -> Option<Option<CommentSide>> {
        match row {
            Row::CommentHeader { half, .. }
            | Row::CommentBody { half, .. }
            | Row::CommentActions { half, .. } => Some(*half),
            _ => None,
        }
    }

    #[test]
    fn split_comments_render_in_the_half_they_were_left_on() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // rows[4..7] are carol's LEFT thread, rows[7..12] the RIGHT one; each
        // row of a thread carries the side it was anchored to.
        let halves: Vec<Option<Option<CommentSide>>> =
            rows[4..12].iter().map(row_half).collect();
        assert_eq!(
            halves,
            vec![
                Some(Some(CommentSide::Left)),  // carol header
                Some(Some(CommentSide::Left)),  // body
                Some(Some(CommentSide::Left)),  // actions
                Some(Some(CommentSide::Right)), // alice header
                Some(Some(CommentSide::Right)), // body
                Some(Some(CommentSide::Right)), // bob reply header
                Some(Some(CommentSide::Right)), // body
                Some(Some(CommentSide::Right)), // actions
            ]
        );
    }

    #[test]
    fn only_a_threads_first_row_draws_the_card_top() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // Exactly one top edge per thread, and it's the root's header — a
        // reply's header sits mid-card and must not cap it.
        let mut tops = 0;
        let mut threads = 0;
        for row in &rows {
            match row {
                Row::CommentHeader { top, is_reply, .. } => {
                    if *top {
                        tops += 1;
                        assert!(!is_reply, "a reply header must not open the card");
                    }
                }
                // The actions row always closes a thread, so it counts them.
                Row::CommentActions { .. } => threads += 1,
                _ => {}
            }
        }
        assert!(threads > 0, "fixture should have threads");
        assert_eq!(tops, threads, "one top edge per thread");
    }

    #[test]
    fn row_height_follows_the_font_size() {
        // The default must reproduce the pre-zoom geometry exactly.
        assert_eq!(DEFAULT_TEXT_SIZE, 13.0);
        assert_eq!(row_height_for(DEFAULT_TEXT_SIZE), 22.0);
        // Rows grow and shrink with the text, always leaving headroom so
        // glyphs can't outgrow their row at either bound.
        assert_eq!(row_height_for(20.), 34.0);
        assert_eq!(row_height_for(8.), 14.0);
        for size in [MIN_TEXT_SIZE, DEFAULT_TEXT_SIZE, MAX_TEXT_SIZE] {
            assert!(
                row_height_for(size) > size,
                "row must be taller than {size}px text"
            );
        }
    }

    #[test]
    fn comment_bodies_wrap_to_the_requested_column_width() {
        // One long prose line plus an unbreakable token (a URL), the two ways a
        // body runs past the card.
        let body = format!("{} https://example.com/{}", "word ".repeat(60), "x".repeat(200));
        let index = group_comments(vec![rc(
            1,
            "a.rs",
            Some("RIGHT"),
            Some(2),
            &body,
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        for (name, mode, cap) in [
            ("unified", ViewMode::Unified, COMMENT_WRAP_CHARS),
            ("split", ViewMode::Split, 40),
        ] {
            let (rows, _, _) =
                build_rows(&sample_diff(), mode, &HashMap::new(), Some(&index), true, cap);
            let bodies: Vec<&SharedString> = rows
                .iter()
                .filter_map(|row| match row {
                    Row::CommentBody { line, .. } => Some(line),
                    _ => None,
                })
                .collect();
            assert!(!bodies.is_empty(), "{name} should have body rows");
            for line in bodies {
                assert!(
                    line.chars().count() <= cap,
                    "{name}: {:?} exceeds {cap} cols",
                    line.as_ref()
                );
            }
        }
    }

    #[test]
    fn unified_comments_span_the_full_width() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // Unified has one column, so no comment row is confined to a half.
        assert!(rows.iter().filter_map(row_half).all(|half| half.is_none()));
        // ...and the diff really does contain comment rows to check.
        assert!(rows.iter().any(is_comment_row));
    }

    #[test]
    fn hidden_comments_keep_header_counts() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            false,
                COMMENT_WRAP_CHARS,
        );
        assert!(!rows.iter().any(is_comment_row));
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (3, 1));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
        // And with no index at all (local items): zero counts, no rows.
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
                COMMENT_WRAP_CHARS,
        );
        assert!(!rows.iter().any(is_comment_row));
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (0, 0));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
    }

    #[test]
    fn comment_anchor_resolves_sides_and_skips_non_lines() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // Headers and comment rows anchor nothing.
        assert_eq!(comment_anchor(&rows, 0, SelSide::Unified), None);
        assert_eq!(comment_anchor(&rows, 4, SelSide::Unified), None);
        // Context row (1,1) → RIGHT 1; removed old1 → LEFT 2; added new1 → RIGHT 2.
        assert_eq!(
            comment_anchor(&rows, 2, SelSide::Unified),
            Some((CommentSide::Right, 1))
        );
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Unified),
            Some((CommentSide::Left, 2))
        );
        assert_eq!(
            comment_anchor(&rows, 8, SelSide::Unified),
            Some((CommentSide::Right, 2))
        );
        // Split: the half under the pointer decides; absent cells refuse.
        let (rows, _, hunk_rows) =
            build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Left),
            Some((CommentSide::Left, 2))
        );
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Right),
            Some((CommentSide::Right, 2))
        );
        // Second hunk: r2 has no right cell (see split tests above).
        let h2 = hunk_rows[1];
        assert_eq!(comment_anchor(&rows, h2 + 2, SelSide::Right), None);
        assert_eq!(
            comment_anchor(&rows, h2 + 2, SelSide::Left),
            Some((CommentSide::Left, 11))
        );
    }

    #[test]
    fn nth_noncomment_row_maps_positions_across_comment_insertions() {
        let index = sample_comments();
        let (plain, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
                COMMENT_WRAP_CHARS,
        );
        let (with, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // Every plain row maps to the same row content with comments shown.
        for (n, row) in plain.iter().enumerate() {
            let ix = nth_noncomment_row(&with, n);
            assert_eq!(row_name(&with[ix]), row_name(row), "row {n}");
        }
        // n beyond the end clamps to the last row.
        assert_eq!(nth_noncomment_row(&plain, 999), plain.len() - 1);
    }
}
