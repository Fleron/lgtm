use crate::{Cell, LineKind, Row};

/// Width of the minimap column on the right edge of the diff pane.
pub(crate) const MINIMAP_WIDTH: f32 = 100.0;
/// Horizontal inset of the bars inside the column.
pub(crate) const MINIMAP_PAD: f32 = 4.0;
/// Gap between the two half-columns mirroring split view.
pub(crate) const MINIMAP_GAP: f32 = 2.0;
/// A line this long (or longer) draws a full-width bar.
const MAX_MINIMAP_CHARS: usize = 160;

/// One display row reduced to what the minimap paints: a kind (color) and a
/// width fraction. Index-aligned with `ItemData::rows`, recomputed wherever
/// the rows are rebuilt.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct MinimapRow {
    pub(crate) kind: MinimapKind,
    /// Line length / MAX_MINIMAP_CHARS, capped at 1. For SplitPair rows this
    /// is the max of the two halves (used by downsample grouping).
    pub(crate) len_frac: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum MinimapKind {
    Context,
    Added,
    Removed,
    /// A split-view row: per-half width fractions, and whether each half
    /// holds a change (left = removed present, right = added present) as
    /// opposed to context. Absent halves have a zero fraction.
    SplitPair {
        left_frac: f32,
        right_frac: f32,
        left: bool,
        right: bool,
    },
    Header,
    Gap,
    Blank,
}

/// Fraction of a full-width minimap bar for one line of text. Counting stops
/// at the cap, so pathological lines cost nothing extra.
pub(crate) fn line_frac(text: &str) -> f32 {
    text.chars().take(MAX_MINIMAP_CHARS).count() as f32 / MAX_MINIMAP_CHARS as f32
}

/// Reduce display rows to minimap rows, one per row, index-aligned.
pub(crate) fn minimap_rows(rows: &[Row]) -> Vec<MinimapRow> {
    rows.iter()
        .map(|row| match row {
            // Comment rows stay blank in the minimap: threads are short and
            // already flagged by the file-header counts.
            Row::Spacer
            | Row::Binary
            | Row::CommentHeader { .. }
            | Row::CommentBody { .. }
            | Row::CommentActions { .. } => MinimapRow {
                kind: MinimapKind::Blank,
                len_frac: 0.,
            },
            Row::FileHeader { .. } | Row::HunkHeader { .. } => MinimapRow {
                kind: MinimapKind::Header,
                len_frac: 1.,
            },
            Row::Gap { .. } => MinimapRow {
                kind: MinimapKind::Gap,
                len_frac: 1.,
            },
            Row::Line { kind, text, .. } => MinimapRow {
                kind: match kind {
                    LineKind::Context => MinimapKind::Context,
                    LineKind::Added => MinimapKind::Added,
                    LineKind::Removed => MinimapKind::Removed,
                },
                len_frac: line_frac(text),
            },
            Row::SplitLine { left, right } => {
                let frac =
                    |cell: &Option<Cell>| cell.as_ref().map(|c| line_frac(&c.text)).unwrap_or(0.);
                let (left_frac, right_frac) = (frac(left), frac(right));
                MinimapRow {
                    kind: MinimapKind::SplitPair {
                        left_frac,
                        right_frac,
                        left: matches!(left, Some(c) if c.kind == LineKind::Removed),
                        right: matches!(right, Some(c) if c.kind == LineKind::Added),
                    },
                    len_frac: left_frac.max(right_frac),
                }
            }
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MinimapLane {
    /// Bar from the left edge across the full usable width (unified rows and
    /// header/gap ticks).
    Full,
    /// Left half-column (split view's left cell).
    Left,
    /// Right half-column (split view's right cell).
    Right,
}

/// Bar colors as a plain enum so the precompute stays theme-free and testable;
/// painting maps them to theme colors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MinimapColor {
    Added,
    Removed,
    Context,
    Header,
    Gap,
}

/// Downsample priority: when several rows share one pixel-row, the highest
/// wins (Blank rows contribute nothing and lose to everything).
fn minimap_priority(color: MinimapColor) -> u8 {
    match color {
        MinimapColor::Removed => 5,
        MinimapColor::Added => 4,
        MinimapColor::Header => 3,
        MinimapColor::Gap => 2,
        MinimapColor::Context => 1,
    }
}

/// A coalesced vertical run of identical bars, in slot space: a slot is one
/// painted row of the minimap, `slot_h` px tall, covering `group` display
/// rows. `tick` runs paint 1px tall at the slot top instead of filling it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct MinimapRun {
    pub(crate) start: u32,
    /// Exclusive.
    pub(crate) end: u32,
    pub(crate) lane: MinimapLane,
    pub(crate) color: MinimapColor,
    pub(crate) frac: f32,
    pub(crate) tick: bool,
}

pub(crate) struct MinimapLayout {
    /// Painted height of one slot: clamp(pane_height / total_rows, 1, 3).
    pub(crate) slot_h: f32,
    /// Display rows per slot; 1 unless the diff is taller than the pane at
    /// 1px per row, then ceil(total / pane_px).
    pub(crate) group: usize,
    pub(crate) runs: Vec<MinimapRun>,
}

pub(crate) fn minimap_scale(total: usize, pane_px: f32) -> (f32, usize) {
    if total == 0 || pane_px <= 0. {
        return (1., 1);
    }
    if total as f32 > pane_px {
        (1., (total as f32 / pane_px).ceil() as usize)
    } else {
        ((pane_px / total as f32).clamp(1., 3.), 1)
    }
}

/// The full minimap precompute: scale, downsample, and coalesce into quad
/// runs. Per group and lane the highest-priority color wins and the width is
/// the group's max fraction; vertically adjacent slots with the same lane,
/// color, and width merge into one run. Recomputed only when the rows are
/// rebuilt or the pane height changes — painting just iterates the runs.
pub(crate) fn minimap_runs(rows: &[MinimapRow], pane_px: f32) -> MinimapLayout {
    let (slot_h, group) = minimap_scale(rows.len(), pane_px);
    let mut runs: Vec<MinimapRun> = Vec::new();
    // Per lane: index into `runs` of the run still open for extension.
    let mut open: [Option<usize>; 3] = [None; 3];
    for (slot, chunk) in rows.chunks(group).enumerate() {
        let mut winner: [Option<MinimapColor>; 3] = [None; 3];
        let mut frac: [f32; 3] = [0.; 3];
        for row in chunk {
            let mut fold = |lane: MinimapLane, color: MinimapColor, f: f32| {
                let ix = lane as usize;
                if winner[ix].map_or(true, |best| {
                    minimap_priority(color) > minimap_priority(best)
                }) {
                    winner[ix] = Some(color);
                }
                frac[ix] = frac[ix].max(f);
            };
            match row.kind {
                MinimapKind::Blank => {}
                MinimapKind::Header => fold(MinimapLane::Full, MinimapColor::Header, 1.),
                MinimapKind::Gap => fold(MinimapLane::Full, MinimapColor::Gap, 1.),
                MinimapKind::Context => {
                    fold(MinimapLane::Full, MinimapColor::Context, row.len_frac)
                }
                MinimapKind::Added => fold(MinimapLane::Full, MinimapColor::Added, row.len_frac),
                MinimapKind::Removed => {
                    fold(MinimapLane::Full, MinimapColor::Removed, row.len_frac)
                }
                MinimapKind::SplitPair {
                    left_frac,
                    right_frac,
                    left,
                    right,
                } => {
                    if left_frac > 0. || left {
                        let color = if left {
                            MinimapColor::Removed
                        } else {
                            MinimapColor::Context
                        };
                        fold(MinimapLane::Left, color, left_frac);
                    }
                    if right_frac > 0. || right {
                        let color = if right {
                            MinimapColor::Added
                        } else {
                            MinimapColor::Context
                        };
                        fold(MinimapLane::Right, color, right_frac);
                    }
                }
            }
        }
        for ix in 0..3 {
            let (Some(color), f) = (winner[ix], frac[ix]) else {
                open[ix] = None;
                continue;
            };
            // Zero-width bars (empty lines) paint nothing; they also break
            // the run so bars on either side don't fuse across them.
            if f <= 0. {
                open[ix] = None;
                continue;
            }
            // Header/gap ticks are 1px marks; when a slot is already 1px
            // they're plain bars and coalesce like everything else.
            let tick = matches!(color, MinimapColor::Header | MinimapColor::Gap) && slot_h > 1.;
            if let Some(run_ix) = open[ix] {
                let run = &mut runs[run_ix];
                if !tick && !run.tick && run.color == color && run.frac == f {
                    run.end += 1;
                    continue;
                }
            }
            open[ix] = (!tick).then_some(runs.len());
            runs.push(MinimapRun {
                start: slot as u32,
                end: slot as u32 + 1,
                lane: [MinimapLane::Full, MinimapLane::Left, MinimapLane::Right][ix],
                color,
                frac: f,
                tick,
            });
        }
    }
    MinimapLayout {
        slot_h,
        group,
        runs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{build_rows, ViewMode};
    use crate::comments::COMMENT_WRAP_CHARS;
    use crate::test_util::{sample_diff, upgraded_diff};
    use std::collections::HashMap;

    fn mrow(kind: MinimapKind, len_frac: f32) -> MinimapRow {
        MinimapRow { kind, len_frac }
    }

    #[test]
    fn minimap_line_frac_caps() {
        assert_eq!(line_frac(""), 0.);
        assert_eq!(line_frac("abcd"), 4. / 160.);
        assert_eq!(line_frac(&"x".repeat(1000)), 1.);
    }

    #[test]
    fn minimap_scale_clamps_and_downsamples() {
        // Tiny diff: 3px max per row, no grouping.
        assert_eq!(minimap_scale(10, 300.), (3., 1));
        // In between: pane / total, no grouping.
        assert_eq!(minimap_scale(200, 300.), (1.5, 1));
        assert_eq!(minimap_scale(300, 300.), (1., 1));
        // Taller than the pane at 1px/row: group N = ceil(total / pane).
        assert_eq!(minimap_scale(1000, 300.), (1., 4));
        assert_eq!(minimap_scale(0, 300.), (1., 1));
    }

    #[test]
    fn minimap_runs_coalesce_same_bars() {
        let rows = vec![
            mrow(MinimapKind::Added, 0.5),
            mrow(MinimapKind::Added, 0.5),
            mrow(MinimapKind::Added, 0.25), // different width: new run
            mrow(MinimapKind::Context, 0.), // empty line: no bar, breaks the run
            mrow(MinimapKind::Added, 0.5),
        ];
        let layout = minimap_runs(&rows, 100.);
        assert_eq!((layout.slot_h, layout.group), (3., 1));
        let expect = |start: u32, end: u32, color: MinimapColor, frac: f32| MinimapRun {
            start,
            end,
            lane: MinimapLane::Full,
            color,
            frac,
            tick: false,
        };
        assert_eq!(
            layout.runs,
            vec![
                expect(0, 2, MinimapColor::Added, 0.5),
                expect(2, 3, MinimapColor::Added, 0.25),
                expect(4, 5, MinimapColor::Added, 0.5),
            ]
        );
    }

    #[test]
    fn minimap_header_ticks_do_not_merge_unless_1px() {
        let rows = vec![
            mrow(MinimapKind::Header, 1.),
            mrow(MinimapKind::Header, 1.),
            mrow(MinimapKind::Context, 0.5),
        ];
        // 3px slots: adjacent headers stay separate 1px ticks.
        let layout = minimap_runs(&rows, 100.);
        assert_eq!(layout.slot_h, 3.);
        assert_eq!(layout.runs.len(), 3);
        assert!(layout.runs[0].tick && layout.runs[1].tick);
        assert_eq!((layout.runs[0].start, layout.runs[0].end), (0, 1));
        assert_eq!((layout.runs[1].start, layout.runs[1].end), (1, 2));
        // 1px slots: ticks are plain bars and coalesce.
        let layout = minimap_runs(&rows, 3.);
        assert_eq!(layout.slot_h, 1.);
        assert_eq!(layout.runs.len(), 2);
        assert!(!layout.runs[0].tick);
        assert_eq!((layout.runs[0].start, layout.runs[0].end), (0, 2));
    }

    #[test]
    fn minimap_downsample_priority_and_max_frac() {
        // 100 rows into a 10px pane: 10 rows per 1px slot.
        let mut rows = vec![mrow(MinimapKind::Blank, 0.); 100];
        rows[0] = mrow(MinimapKind::Removed, 0.1);
        rows[1] = mrow(MinimapKind::Added, 1.);
        rows[2] = mrow(MinimapKind::Context, 0.2);
        for slot in rows[10..20].iter_mut() {
            *slot = mrow(MinimapKind::Context, 0.2);
        }
        let layout = minimap_runs(&rows, 10.);
        assert_eq!((layout.slot_h, layout.group), (1., 10));
        // Slot 0: Removed outranks Added/Context; width is the group max.
        // Slot 1: all-context. Slots 2..: blank, nothing painted.
        assert_eq!(
            layout.runs,
            vec![
                MinimapRun {
                    start: 0,
                    end: 1,
                    lane: MinimapLane::Full,
                    color: MinimapColor::Removed,
                    frac: 1.,
                    tick: false,
                },
                MinimapRun {
                    start: 1,
                    end: 2,
                    lane: MinimapLane::Full,
                    color: MinimapColor::Context,
                    frac: 0.2,
                    tick: false,
                },
            ]
        );
    }

    #[test]
    fn minimap_split_rows_use_half_lanes() {
        let pair = |left_frac: f32, right_frac: f32, left: bool, right: bool| {
            mrow(
                MinimapKind::SplitPair {
                    left_frac,
                    right_frac,
                    left,
                    right,
                },
                left_frac.max(right_frac),
            )
        };
        let rows = vec![
            pair(0.5, 0.25, true, true),
            pair(0.5, 0.25, true, true),
            pair(0.5, 0., false, false), // context left, absent right
        ];
        let layout = minimap_runs(&rows, 90.);
        assert_eq!(layout.slot_h, 3.);
        assert_eq!(
            layout.runs,
            vec![
                MinimapRun {
                    start: 0,
                    end: 2,
                    lane: MinimapLane::Left,
                    color: MinimapColor::Removed,
                    frac: 0.5,
                    tick: false,
                },
                MinimapRun {
                    start: 0,
                    end: 2,
                    lane: MinimapLane::Right,
                    color: MinimapColor::Added,
                    frac: 0.25,
                    tick: false,
                },
                MinimapRun {
                    start: 2,
                    end: 3,
                    lane: MinimapLane::Left,
                    color: MinimapColor::Context,
                    frac: 0.5,
                    tick: false,
                },
            ]
        );
    }

    #[test]
    fn minimap_rows_unified_kinds_and_fracs() {
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
                COMMENT_WRAP_CHARS,
        );
        let mm = minimap_rows(&rows);
        assert_eq!(mm.len(), rows.len());
        // FileHeader and HunkHeader both map to Header ticks.
        assert_eq!(mm[0], mrow(MinimapKind::Header, 1.));
        assert_eq!(mm[1], mrow(MinimapKind::Header, 1.));
        // Line rows: kind from the row, frac = chars / MAX_MINIMAP_CHARS.
        assert_eq!(mm[2], mrow(MinimapKind::Context, 3. / 160.)); // "ctx"
        assert_eq!(mm[3], mrow(MinimapKind::Removed, 4. / 160.)); // "old1"
        assert_eq!(mm[5], mrow(MinimapKind::Added, 4. / 160.)); // "new1"
                                                                // Spacer and Binary rows are blank.
        assert_eq!(mm[14], mrow(MinimapKind::Blank, 0.));
        assert_eq!(mm[16], mrow(MinimapKind::Blank, 0.));
    }

    #[test]
    fn minimap_rows_split_pairs_and_gaps() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let mm = minimap_rows(&rows);
        assert_eq!(mm.len(), rows.len());
        // Context pair: both halves, no change flags.
        assert_eq!(
            mm[2].kind,
            MinimapKind::SplitPair {
                left_frac: 3. / 160.,
                right_frac: 3. / 160.,
                left: false,
                right: false,
            }
        );
        // Paired removed/added ("old1" / "new1").
        assert_eq!(
            mm[3].kind,
            MinimapKind::SplitPair {
                left_frac: 4. / 160.,
                right_frac: 4. / 160.,
                left: true,
                right: true,
            }
        );
        assert_eq!(mm[3].len_frac, 4. / 160.);
        // One-sided rows: the absent half has a zero fraction and no flag.
        let h2 = 6; // second HunkHeader (see split tests above)
        assert_eq!(
            mm[h2 + 2].kind,
            MinimapKind::SplitPair {
                left_frac: 2. / 160., // "r2"
                right_frac: 0.,
                left: true,
                right: false,
            }
        );
        assert_eq!(
            mm[h2 + 4].kind,
            MinimapKind::SplitPair {
                left_frac: 0.,
                right_frac: 4. / 160., // "lone"
                left: false,
                right: true,
            }
        );

        // Gap rows map to Gap in both modes.
        let (diff, upgrades) = upgraded_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, _, _) = build_rows(&diff, mode, &upgrades, None, true, COMMENT_WRAP_CHARS);
            let mm = minimap_rows(&rows);
            assert_eq!(mm[1], mrow(MinimapKind::Gap, 1.));
        }
    }
}
