# Multi-line review comments

## What

Let a reviewer anchor one comment to a range of lines (GitHub's start-line to
end-line comment), not just a single line.

## Why

GitHub supports commenting on a line range; lgtm can already display such
comments (they show up fine when created elsewhere) but has no way to create
one, so reviewers who want to comment on a multi-line block have to leave
lgtm and use GitHub's web UI.

## How

The data model already supports ranges; only the write path doesn't.

`gh::ReviewComment` (`crates/gh/src/lib.rs:169-184`) already has `start_line:
Option<u64>` with a doc comment noting "Multi-line comments start here and
anchor at `line` (the end), like GitHub's own UI" — this is populated when
reading existing comments, never when creating one.

Every comment-creation path is single-line only:
- `LocalReview::add_comment` (`main.rs:495-517`) hardcodes `start_line:
  None` on the draft `ReviewComment` it builds.
- `gh::post_review_comment` (`gh/lib.rs:291-320`) takes `line: u64` and has
  no `start_line`/`start_side` parameter at all; its POST body to
  `repos/.../pulls/.../comments` never sends `start_line`.
- The hover "+" affordance (`render_plus`, `main.rs:7024`) resolves a single
  `(anchor_side, line)` pair via `comment_anchor` (`main.rs:4277`) for
  exactly the row it's rendered on.

Steps:
1. Thread an optional `start_line: Option<u64>` (and `start_side` if it can
   differ from `side`, per GitHub's API) through the write path:
   `gh::post_review_comment` → `add_comment`/`add_local_comment`
   (`main.rs:6273`) → `open_composer` (`main.rs:6065`).
2. `gh::post_review_comment`: when `start_line` is `Some`, add `-F
   start_line={start_line}` and `-F start_side={side}` to the existing `gh
   api` POST call, matching GitHub's multi-line comment fields.
3. UI: add a second interaction to extend the anchor into a range, e.g.
   shift-click a second row's "+" (or drag from the first "+") before
   `open_composer` runs, using the existing `comment_anchor` resolution for
   the second row the same way it's used for the first.

Effort: small to medium. No new API surface or struct fields (both already
exist); the work is wiring the optional range through ~4 existing call sites
and one new interaction gesture in the diff view.
