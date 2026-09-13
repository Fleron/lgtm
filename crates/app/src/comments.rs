// --- Review comments -------------------------------------------------------

use crate::selection::SelSide;
use crate::{row_height, CardEdge, LineKind, Row, ViewMode, SPLIT_DIVIDER};
use gpui::{div, prelude::*, px};
use crate::theme;
use std::collections::HashMap;
use std::ops::Range;

/// Which diff side a review comment anchors to, GitHub's LEFT/RIGHT.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CommentSide {
    Left,
    Right,
}

impl CommentSide {
    pub(crate) fn api_str(self) -> &'static str {
        match self {
            CommentSide::Left => "LEFT",
            CommentSide::Right => "RIGHT",
        }
    }
}

/// One review thread: the top-level comment plus its replies, in
/// created_at order.
#[derive(Debug)]
pub(crate) struct CommentThread {
    pub(crate) root: gh::ReviewComment,
    pub(crate) replies: Vec<gh::ReviewComment>,
}

/// One file's threads, keyed by anchor.
pub(crate) type FileAnchors = HashMap<(CommentSide, u64), Vec<CommentThread>>;

/// Review comments grouped for row building.
#[derive(Debug, Default)]
pub(crate) struct CommentIndex {
    /// path → (side, line) → threads in root-created order.
    pub(crate) threads: HashMap<String, FileAnchors>,
    /// path → (anchored comment count, outdated comment count).
    pub(crate) counts: HashMap<String, (usize, usize)>,
}

/// Group flat REST comments into anchored threads: replies attach to their
/// thread via `in_reply_to_id` (orphans are dropped), threads whose root has
/// no current line are only counted as outdated.
pub(crate) fn group_comments(mut comments: Vec<gh::ReviewComment>) -> CommentIndex {
    // ISO-8601 UTC strings sort lexicographically = chronologically, so this
    // orders roots before their replies and threads by root creation.
    comments.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let mut threads: Vec<CommentThread> = Vec::new();
    // Comment id (root or reply) → thread index, so replies-to-replies still
    // land in the right thread.
    let mut thread_of: HashMap<u64, usize> = HashMap::new();
    for comment in comments {
        match comment.in_reply_to_id {
            None => {
                thread_of.insert(comment.id, threads.len());
                threads.push(CommentThread {
                    root: comment,
                    replies: Vec::new(),
                });
            }
            Some(parent) => {
                if let Some(&ix) = thread_of.get(&parent) {
                    thread_of.insert(comment.id, ix);
                    threads[ix].replies.push(comment);
                }
                // Orphaned reply (parent not fetched): skip.
            }
        }
    }
    let mut index = CommentIndex::default();
    for thread in threads {
        let size = 1 + thread.replies.len();
        let counts = index.counts.entry(thread.root.path.clone()).or_default();
        let Some(line) = thread.root.line else {
            counts.1 += size;
            continue;
        };
        counts.0 += size;
        let side = if thread.root.side.as_deref() == Some("LEFT") {
            CommentSide::Left
        } else {
            CommentSide::Right
        };
        index
            .threads
            .entry(thread.root.path.clone())
            .or_default()
            .entry((side, line))
            .or_default()
            .push(thread);
    }
    index
}

#[derive(Debug, Default)]
pub(crate) struct LocalReview {
    pub(crate) comments: Vec<gh::ReviewComment>,
    next_id: u64,
}

impl LocalReview {
    pub(crate) fn index(&self) -> CommentIndex {
        group_comments(self.comments.clone())
    }

    pub(crate) fn add_comment(
        &mut self,
        reply_to: Option<u64>,
        path: String,
        side: CommentSide,
        line: u64,
        start_line: Option<u64>,
        body: String,
    ) {
        self.next_id += 1;
        self.comments.push(gh::ReviewComment {
            id: self.next_id,
            path,
            line: Some(line),
            side: Some(side.api_str().to_string()),
            start_line,
            body,
            user: gh::Author {
                login: "you".to_string(),
            },
            created_at: "draft".to_string(),
            in_reply_to_id: reply_to,
        });
    }
}

/// Widest a comment body ever wraps, however roomy the pane: past ~80 columns
/// prose gets hard to track. Also the fallback before the pane is measured.
pub(crate) const COMMENT_WRAP_CHARS: usize = 80;
/// Never wrap narrower than this, however cramped the pane — below it every
/// word lands on its own line, which is worse than clipping.
const MIN_COMMENT_WRAP_CHARS: usize = 24;

/// Chrome around a comment body inside its card: the gutter indent, the accent
/// border, and the horizontal padding (see `comment_row`).
pub(crate) const COMMENT_CARD_CHROME: f32 = 72. + 2. + 24.;

/// Columns a comment body can use given the measured list width — the whole
/// width in unified, one half in split. Wrapping is then only as narrow as the
/// pane forces, capped at [`COMMENT_WRAP_CHARS`] for readability.
pub(crate) fn comment_wrap_cols(list_width: f32, mode: ViewMode, char_width: f32) -> usize {
    if char_width <= 0. {
        return COMMENT_WRAP_CHARS;
    }
    let card = match mode {
        ViewMode::Unified => list_width,
        ViewMode::Split => (list_width - SPLIT_DIVIDER) / 2.,
    };
    let cols = ((card - COMMENT_CARD_CHROME) / char_width).floor();
    (cols.max(0.) as usize).clamp(MIN_COMMENT_WRAP_CHARS, COMMENT_WRAP_CHARS)
}

/// Soft-wrap `text` at `width` chars: explicit newlines are preserved, wraps
/// prefer the last space in range (the space is consumed), and a word longer
/// than the width hard-breaks on a char boundary.
pub(crate) fn wrap_body(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let mut line = line.strip_suffix('\r').unwrap_or(line);
        loop {
            // Byte offset of the (width+1)-th char; absent = the rest fits.
            let Some((cut, _)) = line.char_indices().nth(width) else {
                out.push(line.to_string());
                break;
            };
            match line[..cut].rfind(' ') {
                Some(space) if space > 0 => {
                    out.push(line[..space].to_string());
                    line = &line[space + 1..];
                }
                _ => {
                    out.push(line[..cut].to_string());
                    line = &line[cut..];
                }
            }
        }
    }
    out
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Unix seconds for an ISO-8601 UTC timestamp ("2026-07-01T12:34:56Z").
pub(crate) fn parse_iso_utc(s: &str) -> Option<i64> {
    if s.len() < 20 {
        return None;
    }
    let num = |range: Range<usize>| s.get(range)?.parse::<i64>().ok();
    Some(
        days_from_civil(num(0..4)?, num(5..7)?, num(8..10)?) * 86400
            + num(11..13)? * 3600
            + num(14..16)? * 60
            + num(17..19)?,
    )
}

/// Compact "3d ago"-style age of an ISO timestamp relative to `now` (unix
/// seconds). Unparseable input renders as-is.
pub(crate) fn short_age(iso: &str, now: i64) -> String {
    let Some(t) = parse_iso_utc(iso) else {
        return iso.to_string();
    };
    let d = now - t;
    if d < 60 {
        "just now".to_string()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else if d < 365 * 86400 {
        format!("{}d ago", d / 86400)
    } else {
        format!("{}y ago", d / (365 * 86400))
    }
}

/// Append the display rows of every thread anchored at (side, no) — comment
/// headers, wrapped body lines, and one reply-affordance row per thread.
/// No-op when comments are hidden/absent (`anchors` None) or the row has no
/// number on that side.
pub(crate) fn push_thread_rows(
    rows: &mut Vec<Row>,
    anchors: Option<&FileAnchors>,
    path: &str,
    side: CommentSide,
    no: Option<u32>,
    now: i64,
    mode: ViewMode,
    wrap: usize,
) {
    // Split mode renders the thread inside the half it was left on, like the
    // GitHub UI; unified has one column, so the card spans it.
    let half = match mode {
        ViewMode::Split => Some(side),
        ViewMode::Unified => None,
    };
    let (Some(anchors), Some(no)) = (anchors, no) else {
        return;
    };
    let Some(threads) = anchors.get(&(side, no as u64)) else {
        return;
    };
    for thread in threads {
        for (ix, comment) in std::iter::once(&thread.root)
            .chain(&thread.replies)
            .enumerate()
        {
            rows.push(Row::CommentHeader {
                author: comment.user.login.clone().into(),
                when: short_age(&comment.created_at, now).into(),
                is_reply: ix > 0,
                half,
                // A thread always opens with its root's header and closes with
                // the actions row, so those two carry the card's edges.
                top: ix == 0,
            });
            for line in wrap_body(&comment.body, wrap) {
                rows.push(Row::CommentBody {
                    line: line.into(),
                    half,
                });
            }
        }
        rows.push(Row::CommentActions {
            root_id: thread.root.id,
            path: path.to_string().into(),
            side,
            line: no as u64,
            half,
        });
    }
}

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn comment_row(
    inner: gpui::AnyElement,
    half: Option<CommentSide>,
    header: bool,
    edge: CardEdge,
) -> gpui::AnyElement {
    // The card itself: indented past the gutter, blue-tinted and accented so a
    // thread reads as one block distinct from the diff behind it. The author
    // line gets a stronger wash so each comment's start is obvious.
    let card = |inner: gpui::AnyElement| {
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .child(div().w(px(72.)).flex_shrink_0())
            .child({
                // Sides on every row, caps only on the thread's first and last,
                // so the rows stack into one unbroken outline.
                let body = div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(if header {
                        theme::comment_header_bg()
                    } else {
                        theme::comment_bg()
                    })
                    .border_color(theme::comment_outline())
                    .border_l_2()
                    .border_r_1();
                let body = match edge {
                    CardEdge::Top => body.border_t_1().rounded_t_md(),
                    CardEdge::Bottom => body.border_b_1().rounded_b_md(),
                    CardEdge::Middle => body,
                };
                body.px_3()
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .child(inner)
            })
    };
    let row = div().h(px(row_height())).w_full().flex();
    match half {
        // Unified: one column, so the card spans it.
        None => row.child(card(inner)).into_any_element(),
        // Split: sit in the half the comment was left on, mirroring the line
        // rows' [cell | divider | cell] geometry so the divider stays aligned.
        Some(side) => {
            let empty = || div().flex_1().min_w_0().h_full();
            let divider = div()
                .w(px(SPLIT_DIVIDER))
                .flex_shrink_0()
                .h_full()
                .bg(theme::crust())
                .border_l_1()
                .border_r_1()
                .border_color(theme::surface0());
            match side {
                CommentSide::Left => row
                    .child(card(inner))
                    .child(divider)
                    .child(empty())
                    .into_any_element(),
                CommentSide::Right => row
                    .child(empty())
                    .child(divider)
                    .child(card(inner))
                    .into_any_element(),
            }
        }
    }
}

/// The comment anchor of a display row: which (side, line) a new comment on
/// it targets. Unified rows anchor by kind (Removed → LEFT, else RIGHT);
/// split rows by the half under the pointer. Rows without line numbers and
/// absent cells yield None.
pub(crate) fn comment_anchor(rows: &[Row], row_ix: usize, side: SelSide) -> Option<(CommentSide, u64)> {
    match (rows.get(row_ix)?, side) {
        (
            Row::Line {
                kind: LineKind::Removed,
                old_no,
                ..
            },
            _,
        ) => Some((CommentSide::Left, (*old_no)? as u64)),
        (Row::Line { new_no, .. }, _) => Some((CommentSide::Right, (*new_no)? as u64)),
        (Row::SplitLine { left, .. }, SelSide::Left) => {
            Some((CommentSide::Left, left.as_ref()?.no as u64))
        }
        (Row::SplitLine { right, .. }, SelSide::Right) => {
            Some((CommentSide::Right, right.as_ref()?.no as u64))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    #[test]
    fn wrap_preserves_newlines_and_breaks_at_spaces() {
        // Explicit newlines (and CRLF) survive; empty lines stay empty.
        assert_eq!(wrap_body("a\n\nb\r\nc", 10), vec!["a", "", "b", "c"]);
        // Fits exactly: no wrap.
        assert_eq!(wrap_body("abcde", 5), vec!["abcde"]);
        // Breaks at the last space in range; the space is consumed.
        assert_eq!(
            wrap_body("one two three four", 9),
            vec!["one two", "three", "four"]
        );
        // A word longer than the width hard-breaks; no space is invented.
        assert_eq!(wrap_body("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // Never splits inside a word when a space exists in range.
        assert_eq!(wrap_body("aa bbbb", 5), vec!["aa", "bbbb"]);
        // Multibyte chars: wraps on char boundaries, not bytes.
        assert_eq!(wrap_body("ééééé", 3), vec!["ééé", "éé"]);
        assert_eq!(wrap_body("éé éé", 3), vec!["éé", "éé"]);
    }

    #[test]
    fn short_age_buckets() {
        let now = parse_iso_utc("2026-07-09T12:00:00Z").unwrap();
        assert_eq!(short_age("2026-07-09T11:59:30Z", now), "just now");
        assert_eq!(short_age("2026-07-09T11:15:00Z", now), "45m ago");
        assert_eq!(short_age("2026-07-09T05:00:00Z", now), "7h ago");
        assert_eq!(short_age("2026-07-06T12:00:00Z", now), "3d ago");
        assert_eq!(short_age("2024-01-01T00:00:00Z", now), "2y ago");
        // Clock skew (future timestamp) degrades to "just now".
        assert_eq!(short_age("2026-07-09T12:05:00Z", now), "just now");
        // Unparseable input renders as-is.
        assert_eq!(short_age("garbage", now), "garbage");
    }

    #[test]
    fn parse_iso_utc_matches_known_epoch() {
        assert_eq!(parse_iso_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_utc("2001-09-09T01:46:40Z"), Some(1_000_000_000));
        assert_eq!(parse_iso_utc("2026-07-09"), None);
    }

    #[test]
    fn group_comments_threads_replies_orphans_and_outdated() {
        let index = group_comments(vec![
            // Deliberately out of order: grouping sorts by created_at.
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
                1,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "root",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            // Second thread at the same anchor, created later.
            rc(
                3,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "later",
                "carol",
                "2026-01-03T00:00:00Z",
                None,
            ),
            // LEFT-side thread on the same line number: distinct anchor.
            rc(
                4,
                "a.rs",
                Some("LEFT"),
                Some(2),
                "old side",
                "dave",
                "2026-01-04T00:00:00Z",
                None,
            ),
            // Outdated: no current line. Counts, never anchors.
            rc(
                5,
                "a.rs",
                None,
                None,
                "stale",
                "erin",
                "2026-01-05T00:00:00Z",
                None,
            ),
            // Orphaned reply: parent never fetched — dropped entirely.
            rc(
                6,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "orphan",
                "mallory",
                "2026-01-06T00:00:00Z",
                Some(999),
            ),
            // Reply-to-a-reply lands in the root's thread.
            rc(
                7,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "nested",
                "alice",
                "2026-01-07T00:00:00Z",
                Some(2),
            ),
            rc(
                8,
                "b.rs",
                Some("RIGHT"),
                Some(9),
                "other file",
                "bob",
                "2026-01-08T00:00:00Z",
                None,
            ),
        ]);
        let a = &index.threads["a.rs"];
        let right = &a[&(CommentSide::Right, 2)];
        assert_eq!(right.len(), 2);
        assert_eq!(right[0].root.id, 1);
        assert_eq!(
            right[0].replies.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![2, 7]
        );
        assert_eq!(right[1].root.id, 3);
        assert!(right[1].replies.is_empty());
        assert_eq!(a[&(CommentSide::Left, 2)][0].root.id, 4);
        // Counts: anchored = 3 (root+reply+nested) + 1 + 1 = 5 (the orphan
        // is dropped), outdated = 1.
        assert_eq!(index.counts["a.rs"], (5, 1));
        assert_eq!(index.counts["b.rs"], (1, 0));
        assert_eq!(index.threads["b.rs"][&(CommentSide::Right, 9)].len(), 1);
    }

    #[test]
    fn comment_wrap_cols_follows_the_pane_width() {
        let cw = 8.0; // 8px per column keeps the arithmetic obvious.
        let chrome = COMMENT_CARD_CHROME;
        // Unified uses the whole width; split only its half, so the same pane
        // yields roughly half the columns.
        assert_eq!(comment_wrap_cols(chrome + 40. * cw, ViewMode::Unified, cw), 40);
        let split_pane = 2. * (chrome + 40. * cw) + SPLIT_DIVIDER;
        assert_eq!(comment_wrap_cols(split_pane, ViewMode::Split, cw), 40);
        // A roomy pane stops widening at the readability cap...
        assert_eq!(
            comment_wrap_cols(10_000., ViewMode::Unified, cw),
            COMMENT_WRAP_CHARS
        );
        // ...and a cramped one bottoms out rather than wrapping every word.
        assert_eq!(
            comment_wrap_cols(0., ViewMode::Split, cw),
            MIN_COMMENT_WRAP_CHARS
        );
        // Degenerate char width can't divide by zero.
        assert_eq!(
            comment_wrap_cols(800., ViewMode::Unified, 0.),
            COMMENT_WRAP_CHARS
        );
    }

    #[test]
    fn local_review_comments_index_like_review_threads() {
        let mut review = LocalReview::default();
        review.add_comment(
            None,
            "src/lib.rs".to_string(),
            CommentSide::Right,
            12,
            None,
            "please simplify this".to_string(),
        );
        review.add_comment(
            Some(1),
            "src/lib.rs".to_string(),
            CommentSide::Right,
            12,
            None,
            "also add a test".to_string(),
        );

        let index = review.index();
        assert_eq!(index.counts["src/lib.rs"], (2, 0));
        let threads = &index.threads["src/lib.rs"][&(CommentSide::Right, 12)];
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].root.body, "please simplify this");
        assert_eq!(threads[0].replies[0].body, "also add a test");
    }

}
