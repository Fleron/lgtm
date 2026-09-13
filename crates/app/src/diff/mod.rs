mod gaps;
mod highlight;

pub(crate) use gaps::{push_gap_rows, run_upgrade, FileUpgrade, UpgradeJob, UpgradeSource};
#[cfg(test)]
pub(crate) use gaps::gap_span;
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

