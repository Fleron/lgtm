use crate::Row;
use diff_core::PrDiff;
use std::ops::Range;

/// Which text stream a selection runs through. Split selections are locked to
/// the side where the drag started, like GitHub; the other side paints nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SelSide {
    Unified,
    Left,
    Right,
}

/// A point in text space: display row index + char index into that row's text
/// (char, not byte — convert to byte offsets only when slicing). Ordered by
/// (row, col), which is exactly document order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct RowCol {
    pub(crate) row: usize,
    pub(crate) col: usize,
}

/// Anchor stays where the drag started; head follows the mouse. The ordered
/// pair is derived on use, so dragging upward needs no special casing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Selection {
    pub(crate) side: SelSide,
    pub(crate) anchor: RowCol,
    pub(crate) head: RowCol,
}

impl Selection {
    pub(crate) fn ordered(&self) -> (RowCol, RowCol) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// The text a selection on `side` runs through for this row, if any. Rows
/// that aren't text (headers, spacers, binary) and absent split cells yield
/// None: they're selectable-through but contribute nothing.
pub(crate) fn row_side_text(row: &Row, side: SelSide) -> Option<&str> {
    match (row, side) {
        (Row::Line { text, .. }, SelSide::Unified) => Some(text.as_ref()),
        (Row::SplitLine { left, .. }, SelSide::Left) => left.as_ref().map(|c| c.text.as_ref()),
        (Row::SplitLine { right, .. }, SelSide::Right) => right.as_ref().map(|c| c.text.as_ref()),
        _ => None,
    }
}

/// Byte offset of char index `col`, clamped to the end of the text.
pub(crate) fn char_to_byte(text: &str, col: usize) -> usize {
    text.char_indices()
        .nth(col)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// The selected byte range within display row `row_ix`, or None if the row
/// contributes nothing (outside the selection, not a text row, or the selected
/// side is absent). The range can be empty (e.g. a selected empty line): copy
/// keeps it as an empty line, painting skips it.
pub(crate) fn row_selection_range(sel: &Selection, row_ix: usize, row: &Row) -> Option<Range<usize>> {
    let (start, end) = sel.ordered();
    if row_ix < start.row || row_ix > end.row {
        return None;
    }
    let text = row_side_text(row, sel.side)?;
    let chars = text.chars().count();
    let start_col = if row_ix == start.row {
        start.col.min(chars)
    } else {
        0
    };
    let end_col = if row_ix == end.row {
        end.col.min(chars)
    } else {
        chars
    };
    if start_col > end_col {
        return None;
    }
    Some(char_to_byte(text, start_col)..char_to_byte(text, end_col))
}

/// The selected text: each contributing row's selected substring, joined with
/// newlines. Header/spacer rows and absent split cells are skipped entirely
/// (no blank line for them).
pub(crate) fn selection_text(sel: &Selection, rows: &[Row]) -> String {
    let (start, end) = sel.ordered();
    let mut parts = Vec::new();
    for ix in start.row..=end.row.min(rows.len().saturating_sub(1)) {
        if let Some(range) = row_selection_range(sel, ix, &rows[ix]) {
            let text = row_side_text(&rows[ix], sel.side).unwrap_or_default();
            parts.push(&text[range]);
        }
    }
    parts.join("\n")
}

/// What a selection pins down for the chat: the anchor triple (path, side,
/// line range) plus the selected text.
#[derive(Debug, PartialEq)]
pub(crate) struct SelectionInfo {
    pub(crate) path: String,
    pub(crate) side: &'static str,
    pub(crate) lo: u32,
    pub(crate) hi: u32,
    pub(crate) text: String,
}

impl SelectionInfo {
    pub(crate) fn block(&self) -> String {
        format!(
            "Selected text ({}:{}-{}, {} side):\n```\n{}\n```\n",
            self.path, self.lo, self.hi, self.side, self.text
        )
    }

    pub(crate) fn note(&self) -> String {
        format!(
            "› included selection: {}:{}-{}",
            self.path, self.lo, self.hi
        )
    }
}

/// Resolve a selection to its anchor info, reusing the same row machinery as
/// copy. Line numbers come from the selected side (unified rows prefer the
/// new number); the path is the file containing the selection's start row.
/// None when the selection has no text.
pub(crate) fn selection_info(
    sel: &Selection,
    rows: &[Row],
    file_rows: &[usize],
    diff: &PrDiff,
) -> Option<SelectionInfo> {
    let text = selection_text(sel, rows);
    if text.is_empty() {
        return None;
    }
    let (start, end) = sel.ordered();
    let file_ix = file_rows.iter().rposition(|&ix| ix <= start.row)?;
    let path = diff.files.get(file_ix)?.display_path().to_string();
    let (mut lo, mut hi) = (u32::MAX, 0);
    for ix in start.row..=end.row.min(rows.len().saturating_sub(1)) {
        if row_selection_range(sel, ix, &rows[ix]).is_none() {
            continue;
        }
        let no = match (&rows[ix], sel.side) {
            (Row::Line { old_no, new_no, .. }, _) => new_no.or(*old_no),
            (Row::SplitLine { left, .. }, SelSide::Left) => left.as_ref().map(|c| c.no),
            (Row::SplitLine { right, .. }, SelSide::Right) => right.as_ref().map(|c| c.no),
            _ => None,
        };
        if let Some(no) = no {
            lo = lo.min(no);
            hi = hi.max(no);
        }
    }
    if lo == u32::MAX {
        return None;
    }
    let side = match sel.side {
        SelSide::Unified => "unified",
        SelSide::Left => "LEFT (old)",
        SelSide::Right => "RIGHT (new)",
    };
    Some(SelectionInfo {
        path,
        side,
        lo,
        hi,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use crate::comments::COMMENT_WRAP_CHARS;
    use crate::diff::build_rows;
    use crate::{Cell, LineKind, ViewMode};
    use std::collections::HashMap;

    fn line(text: &str) -> Row {
        Row::Line {
            old_no: Some(1),
            new_no: Some(1),
            kind: LineKind::Context,
            text: text.to_string().into(),
            intra: Vec::new(),
            syntax: Vec::new(),
        }
    }

    fn split(left: Option<&str>, right: Option<&str>) -> Row {
        let cell = |text: &str| Cell {
            no: 1,
            kind: LineKind::Context,
            text: text.to_string().into(),
            intra: Vec::new(),
            syntax: Vec::new(),
        };
        Row::SplitLine {
            left: left.map(cell),
            right: right.map(cell),
        }
    }

    fn sel(side: SelSide, anchor: (usize, usize), head: (usize, usize)) -> Selection {
        Selection {
            side,
            anchor: RowCol {
                row: anchor.0,
                col: anchor.1,
            },
            head: RowCol {
                row: head.0,
                col: head.1,
            },
        }
    }

    #[test]
    fn selection_ordered_swaps_backward_drags() {
        let forward = sel(SelSide::Unified, (1, 3), (4, 2));
        let backward = sel(SelSide::Unified, (4, 2), (1, 3));
        assert_eq!(forward.ordered(), backward.ordered());
        // Same row, backward drag: ordered by column.
        let same_row = sel(SelSide::Unified, (2, 7), (2, 1));
        let (start, end) = same_row.ordered();
        assert_eq!((start.col, end.col), (1, 7));
    }

    #[test]
    fn char_to_byte_multibyte() {
        let text = "let s = \"héllo\";";
        assert_eq!(char_to_byte(text, 0), 0);
        assert_eq!(char_to_byte(text, 10), 10); // 'é'
        assert_eq!(char_to_byte(text, 11), 12); // first 'l': 'é' is 2 bytes
        assert_eq!(char_to_byte(text, 99), text.len()); // clamped
        assert_eq!(
            &text[char_to_byte(text, 8)..char_to_byte(text, 14)],
            "\"héllo"
        );
    }

    #[test]
    fn row_range_unified_multi_row() {
        let rows = vec![line("first line"), line("middle"), line("last line")];
        let sel = sel(SelSide::Unified, (0, 6), (2, 4));
        // First row: from col 6 to end of text.
        assert_eq!(row_selection_range(&sel, 0, &rows[0]), Some(6..10));
        // Middle row: fully selected, col 0 to end.
        assert_eq!(row_selection_range(&sel, 1, &rows[1]), Some(0..6));
        // Last row: col 0 to col 4.
        assert_eq!(row_selection_range(&sel, 2, &rows[2]), Some(0..4));
        // Outside the selection.
        assert_eq!(row_selection_range(&sel, 3, &rows[0]), None);
    }

    #[test]
    fn row_range_skips_non_text_rows() {
        let header = Row::HunkHeader {
            label: "@@".into(),
            upgraded: false,
        };
        let sel = sel(SelSide::Unified, (0, 0), (2, 3));
        // Headers/spacers inside the span contribute nothing…
        assert_eq!(row_selection_range(&sel, 1, &header), None);
        assert_eq!(row_selection_range(&sel, 1, &Row::Spacer), None);
        // …and split rows never match a Unified-side selection.
        assert_eq!(
            row_selection_range(&sel, 1, &split(Some("x"), Some("y"))),
            None
        );
    }

    #[test]
    fn row_range_split_sides_and_absent_cells() {
        let rows = vec![
            split(Some("left one"), Some("right one")),
            split(None, Some("right only")),
            split(Some("left only"), None),
        ];
        let right = sel(SelSide::Right, (0, 6), (2, 4));
        assert_eq!(row_selection_range(&right, 0, &rows[0]), Some(6..9));
        assert_eq!(row_selection_range(&right, 1, &rows[1]), Some(0..10));
        // Selected side absent: contributes nothing.
        assert_eq!(row_selection_range(&right, 2, &rows[2]), None);
        let left = sel(SelSide::Left, (0, 5), (1, 3));
        assert_eq!(row_selection_range(&left, 0, &rows[0]), Some(5..8));
        assert_eq!(row_selection_range(&left, 1, &rows[1]), None);
    }

    #[test]
    fn row_range_clamps_columns_to_text() {
        let rows = vec![line("ab"), line("cdef")];
        // Anchor col way past the end of a short line.
        let sel = sel(SelSide::Unified, (0, 99), (1, 2));
        assert_eq!(row_selection_range(&sel, 0, &rows[0]), Some(2..2)); // empty
        assert_eq!(row_selection_range(&sel, 1, &rows[1]), Some(0..2));
    }

    #[test]
    fn copy_assembles_contributing_rows() {
        let rows = vec![
            line("fn main() {"),
            Row::HunkHeader {
                label: "@@".into(),
                upgraded: false,
            },
            line(""),
            line("    body();"),
            line("}"),
        ];
        // From col 3 of row 0 through col 1 of row 4: the header is skipped
        // (no blank line for it), the empty line survives as an empty line.
        let forward = sel(SelSide::Unified, (0, 3), (4, 1));
        assert_eq!(
            selection_text(&forward, &rows),
            "main() {\n\n    body();\n}"
        );
        // Backward drag over one row.
        let backward = sel(SelSide::Unified, (0, 7), (0, 3));
        assert_eq!(selection_text(&backward, &rows), "main");
    }

    #[test]
    fn copy_split_takes_locked_side_only() {
        let rows = vec![
            split(Some("old a"), Some("new a")),
            split(None, Some("new b")),
            split(Some("old c"), None),
        ];
        let right = sel(SelSide::Right, (0, 0), (2, 5));
        assert_eq!(selection_text(&right, &rows), "new a\nnew b");
        let left = sel(SelSide::Left, (0, 0), (2, 5));
        assert_eq!(selection_text(&left, &rows), "old a\nold c");
    }

    #[test]
    fn copy_multibyte_slice() {
        let rows = vec![line("let s = \"héllo\";")];
        let quoted = sel(SelSide::Unified, (0, 8), (0, 14));
        assert_eq!(selection_text(&quoted, &rows), "\"héllo");
    }

    #[test]
    fn selection_info_resolves_anchor_and_text() {
        let (rows, file_rows, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
                COMMENT_WRAP_CHARS,
        );
        // rows: FileHeader, HunkHeader, ctx(1,1 "ctx"), rem(2 "old1"),
        // rem(3 "old2"), add(2 "new1"), …
        let unified = sel(SelSide::Unified, (2, 0), (5, 4));
        let info = selection_info(&unified, &rows, &file_rows, &sample_diff()).unwrap();
        assert_eq!(info.path, "a.rs");
        assert_eq!(info.side, "unified");
        // ctx new_no 1 .. rem old2 old_no 3 / add new1 new_no 2 → lo 1, hi 3.
        assert_eq!((info.lo, info.hi), (1, 3));
        assert_eq!(info.text, "ctx\nold1\nold2\nnew1");
        assert!(info.block().contains("a.rs:1-3, unified side"));
        assert_eq!(info.note(), "› included selection: a.rs:1-3");

        // Split selection locked to the right side.
        let (rows, file_rows, _) =
            build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let right = sel(SelSide::Right, (3, 0), (4, 4));
        let info = selection_info(&right, &rows, &file_rows, &sample_diff()).unwrap();
        assert_eq!(info.side, "RIGHT (new)");
        assert_eq!((info.lo, info.hi), (2, 3));
        assert_eq!(info.text, "new1\nnew2");

        // A selection with no text (headers only) yields nothing.
        let empty = sel(SelSide::Unified, (0, 0), (0, 5));
        assert_eq!(
            selection_info(&empty, &rows, &file_rows, &sample_diff()),
            None
        );
    }

}
