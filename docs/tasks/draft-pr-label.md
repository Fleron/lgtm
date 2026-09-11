# Draft PR label in the titlebar

## What

Show a gray "draft" tag (instead of green "open") in the open-PR titlebar
when the PR is a draft, matching what the cmd-k picker already does.

## Why

GitHub's own PR list distinguishes draft PRs with a gray icon and "Draft"
label. lgtm's cmd-k picker already does this, but the titlebar of an open PR
still just says "open" once you're reviewing a draft, so the state is lost
at exactly the point where it matters most.

## How

`gh::PrSummary` (`crates/gh/src/lib.rs:138-146`) already has `is_draft:
bool`, fetched via `isDraft` in `list_prs`'s `--json` field list
(`gh/lib.rs:160`), and the picker row already renders a gray dot from it.

`gh::PrMeta` (`gh/lib.rs:91-114`), the struct backing the open-PR titlebar,
has no such field, and `fetch_meta`'s `--json` list (`gh/lib.rs:129-130`)
doesn't request `isDraft` either. `pr_titlebar_content` (`main.rs:2922-2935`)
renders the state tag purely from `meta.state`, which stays `"OPEN"` for
drafts, so nothing today can tell them apart there.

Steps:
1. `gh/lib.rs`: add `#[serde(default)] pub is_draft: bool,` to `PrMeta`.
2. `gh/lib.rs:129-130`: append `,isDraft` to `fetch_meta`'s `--json` string.
3. `main.rs:2923-2928`: before matching on `meta.state`, check
   `meta.is_draft` first; if true, use `(theme::overlay0(), "draft")` (the
   same gray already used for the picker dot), otherwise fall through to the
   existing OPEN/MERGED/CLOSED match unchanged.

Effort: small. ~4 lines across 2 files, no new dependencies, reuses the
existing `Tag::custom` rendering and `theme::overlay0()` gray.
