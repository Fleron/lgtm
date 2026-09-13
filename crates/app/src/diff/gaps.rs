use super::highlight::MAX_SYNTAX_LINE_BYTES;
use super::{Cell, LineKind, Row, ViewMode};
use crate::comments::{push_thread_rows, CommentSide, FileAnchors};
use diff_core::{diff_texts, DiffRow, FileStatus, Hunk};
use gpui::SharedString;
use std::collections::HashSet;
use std::ops::Range;

/// Phase-2 result for one file: full new-side lines and whole-file syntax
/// span tables (per line, both sides), plus which gaps the user has expanded.
/// The authoritative re-diffed hunks live in `diff.files[ix].hunks` — this is
/// the extra state needed to render spans and expand context. Lives in
/// `ItemData.upgrades`, so a view-mode toggle preserves expansion and a
/// refresh (which rebuilds ItemData contents) resets it.
pub(crate) struct FileUpgrade {
    /// The complete new-side file, line-ending-normalized, split into lines.
    pub(crate) new_lines: Vec<SharedString>,
    pub(crate) old_spans: Vec<Vec<(Range<usize>, syntax::Token)>>,
    pub(crate) new_spans: Vec<Vec<(Range<usize>, syntax::Token)>>,
    /// Expanded gap indices (0 = before the first hunk, i+1 = after hunk i).
    pub(crate) expanded: HashSet<usize>,
}

impl FileUpgrade {
    /// Whole-file syntax spans for a hunk row, by its side's line number.
    pub(crate) fn row_spans(&self, row: &DiffRow) -> Vec<(Range<usize>, syntax::Token)> {
        let (table, no, text) = match row {
            DiffRow::Context { new_no, text, .. } | DiffRow::Added { new_no, text, .. } => {
                (&self.new_spans, *new_no, text)
            }
            DiffRow::Removed { old_no, text, .. } => (&self.old_spans, *old_no, text),
        };
        if text.len() > MAX_SYNTAX_LINE_BYTES {
            return Vec::new();
        }
        table.get(no as usize - 1).cloned().unwrap_or_default()
    }
}

/// The hidden shared lines in gap `gap_ix` (0..=hunks.len()) of an upgraded
/// file: (first old line, first new line, line count), 1-based. Context lines
/// are shared, so the count is the same on both sides and old numbers stay a
/// constant offset from new ones across the gap. Handles git's zero-count
/// convention (a zero-count side's start is the line *before* the hunk).
pub(crate) fn gap_span(hunks: &[Hunk], gap_ix: usize, total_new: u32) -> (u32, u32, u32) {
    let (old_lo, new_lo) = if gap_ix == 0 {
        (1, 1)
    } else {
        let h = &hunks[gap_ix - 1];
        let pre_old = if h.old_count == 0 {
            h.old_start
        } else {
            h.old_start - 1
        };
        let pre_new = if h.new_count == 0 {
            h.new_start
        } else {
            h.new_start - 1
        };
        (pre_old + h.old_count + 1, pre_new + h.new_count + 1)
    };
    let new_hi = if gap_ix == hunks.len() {
        total_new
    } else {
        let h = &hunks[gap_ix];
        if h.new_count == 0 {
            h.new_start
        } else {
            h.new_start - 1
        }
    };
    (old_lo, new_lo, (new_hi + 1).saturating_sub(new_lo))
}

/// Emit gap `gap_ix` of an upgraded file into `rows`: nothing when no lines
/// are hidden there, synthesized full-context rows when expanded, otherwise
/// one clickable Gap row.
pub(crate) fn push_gap_rows(
    rows: &mut Vec<Row>,
    upgrade: &FileUpgrade,
    hunks: &[Hunk],
    file_ix: usize,
    gap_ix: usize,
    mode: ViewMode,
    path: &str,
    anchors: Option<&FileAnchors>,
    now: i64,
    wrap: usize,
) {
    let (old_lo, new_lo, count) = gap_span(hunks, gap_ix, upgrade.new_lines.len() as u32);
    if count == 0 {
        return;
    }
    if !upgrade.expanded.contains(&gap_ix) {
        rows.push(Row::Gap {
            file_ix,
            gap_ix,
            hidden: count,
        });
        return;
    }
    for j in 0..count {
        let (old_no, new_no) = (old_lo + j, new_lo + j);
        let text = upgrade.new_lines[(new_no - 1) as usize].clone();
        let syntax = if text.len() > MAX_SYNTAX_LINE_BYTES {
            Vec::new()
        } else {
            upgrade
                .new_spans
                .get((new_no - 1) as usize)
                .cloned()
                .unwrap_or_default()
        };
        rows.push(match mode {
            ViewMode::Unified => Row::Line {
                old_no: Some(old_no),
                new_no: Some(new_no),
                kind: LineKind::Context,
                text,
                intra: Vec::new(),
                syntax,
            },
            ViewMode::Split => Row::SplitLine {
                left: Some(Cell {
                    no: old_no,
                    kind: LineKind::Context,
                    text: text.clone(),
                    intra: Vec::new(),
                    syntax: syntax.clone(),
                }),
                right: Some(Cell {
                    no: new_no,
                    kind: LineKind::Context,
                    text,
                    intra: Vec::new(),
                    syntax,
                }),
            },
        });
        push_thread_rows(rows, anchors, path, CommentSide::Left, Some(old_no), now, ViewMode::Split, wrap);
        push_thread_rows(rows, anchors, path, CommentSide::Right, Some(new_no), now, ViewMode::Split, wrap);
    }
}

// --- Phase-2 full-content upgrade ---------------------------------------

/// Never upgrade more than this many files per item; the rest stay on the
/// perfectly serviceable patch-derived view.
pub(crate) const MAX_UPGRADE_FILES: usize = 400;
/// Per-side blob cap; gh::fetch_file_at enforces the same limit for PRs.
pub(crate) const MAX_UPGRADE_BLOB_BYTES: usize = 1024 * 1024;
/// gh/git are one subprocess per call, so a small worker pool is plenty.
pub(crate) const UPGRADE_WORKERS: usize = 4;

/// Where to fetch full file contents from, snapshotted when an item loads.
pub(crate) enum UpgradeSource {
    Pr {
        loc: gh::PrLocator,
        base_oid: String,
        head_oid: String,
    },
    Local(git::LocalSource),
}

/// One file's fetch inputs, snapshotted from the parsed patch (paths and
/// statuses are already known — no extra API call needed).
pub(crate) struct UpgradeJob {
    pub(crate) file_ix: usize,
    pub(crate) old_path: Option<String>,
    pub(crate) new_path: Option<String>,
    pub(crate) status: FileStatus,
}

/// One file's completed upgrade: authoritative hunks plus the render state.
pub(crate) struct UpgradedFile {
    pub(crate) file_ix: usize,
    pub(crate) hunks: Vec<Hunk>,
    pub(crate) additions: u32,
    pub(crate) deletions: u32,
    pub(crate) upgrade: FileUpgrade,
}

/// Blocking fetch + re-diff + whole-file highlight for every eligible file;
/// runs on the background executor. Files that fail on either side are simply
/// missing from the result and keep their patch-derived view.
pub(crate) fn run_upgrade(source: &UpgradeSource, mut jobs: Vec<UpgradeJob>) -> Vec<UpgradedFile> {
    if jobs.len() > MAX_UPGRADE_FILES {
        eprintln!(
            "lgtm: {} changed files; upgrading the first {MAX_UPGRADE_FILES} to full \
             contents, the rest stay patch-derived",
            jobs.len()
        );
        jobs.truncate(MAX_UPGRADE_FILES);
    }
    let queue = std::sync::Mutex::new(jobs.into_iter());
    let done = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..UPGRADE_WORKERS {
            scope.spawn(|| loop {
                let Some(job) = queue.lock().unwrap().next() else {
                    break;
                };
                if let Some(result) = upgrade_file(source, &job) {
                    done.lock().unwrap().push(result);
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by_key(|file| file.file_ix);
    done
}

/// One side's full contents, or None to leave the file un-upgraded: absent,
/// binary/non-UTF-8, over the size cap, or a fetch failure.
fn fetch_side(source: &UpgradeSource, path: &str, old: bool) -> Option<String> {
    let text = match source {
        UpgradeSource::Pr {
            loc,
            base_oid,
            head_oid,
        } => {
            let oid = if old { base_oid } else { head_oid };
            match gh::fetch_file_at(loc, oid, path) {
                Ok(text) => text?,
                Err(err) => {
                    eprintln!("lgtm: {path}: {err:#}");
                    return None;
                }
            }
        }
        UpgradeSource::Local(src) => {
            if old {
                git::file_at_base(src, path)?
            } else {
                String::from_utf8(std::fs::read(src.repo_root.join(path)).ok()?).ok()?
            }
        }
    };
    (text.len() <= MAX_UPGRADE_BLOB_BYTES).then_some(text)
}

pub(crate) fn upgrade_file(source: &UpgradeSource, job: &UpgradeJob) -> Option<UpgradedFile> {
    // Presence must match the patch's story: Added has no old side, Deleted no
    // new side, everything else needs both. A wanted-but-missing side (404,
    // binary, huge) keeps the whole file patch-derived.
    let old_text = match (job.status, &job.old_path) {
        (FileStatus::Added, _) => String::new(),
        (_, Some(path)) => fetch_side(source, path, true)?,
        (_, None) => return None,
    };
    let new_text = match (job.status, &job.new_path) {
        (FileStatus::Deleted, _) => String::new(),
        (_, Some(path)) => fetch_side(source, path, false)?,
        (_, None) => return None,
    };
    // Normalize CRLF→LF up front: hunks, span tables, and gap rows must all
    // index the same line/byte space (diff_texts itself is ending-agnostic).
    let normalize = |text: String| {
        if text.contains('\r') {
            text.replace("\r\n", "\n")
        } else {
            text
        }
    };
    let old_text = normalize(old_text);
    let new_text = normalize(new_text);

    let hunks = diff_texts(&old_text, &new_text, 3);
    let (additions, deletions) =
        hunks
            .iter()
            .flat_map(|hunk| &hunk.rows)
            .fold((0, 0), |(a, d), row| match row {
                DiffRow::Added { .. } => (a + 1, d),
                DiffRow::Removed { .. } => (a, d + 1),
                DiffRow::Context { .. } => (a, d),
            });

    let path = job.new_path.as_deref().or(job.old_path.as_deref())?;
    let lang = syntax::language_for_path(path);
    let spans = |text: &str| match lang {
        Some(lang) if !text.is_empty() => syntax::highlight_lines(lang, text),
        _ => Vec::new(),
    };
    Some(UpgradedFile {
        file_ix: job.file_ix,
        hunks,
        additions,
        deletions,
        upgrade: FileUpgrade {
            old_spans: spans(&old_text),
            new_spans: spans(&new_text),
            new_lines: new_text
                .lines()
                .map(|line| SharedString::from(line.to_string()))
                .collect(),
            expanded: HashSet::new(),
        },
    })
}

