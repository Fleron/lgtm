# Draft: PR overview and summary

Status: Draft / not implementation-ready. The open product decisions below must be resolved before this becomes an executable implementation task.

## Goal

Explore a first slice for showing a concise PR overview above or alongside the diff. Reuse metadata LGTM already fetches so a reviewer can understand the change without leaving the review, while using the state, check, and review-attention mapping proposed by [task 2](task-2.md).

## Current support

- `gh::fetch_meta` already fetches the PR title, number, state, author login, description/body, base and head branches, additions, deletions, changed-file count, and `reviewDecision` through `PrMeta`.
- The PR body is included in the existing chat context by `pr_chat_header`/`chat_prompt`, but it is not rendered as an overview in the review UI.
- The active PR title bar already renders a compact subset of this metadata; the diff and inline review-comment rows are the main review surface.

## Proposed first slice

Candidate overview content, placed above or alongside the diff after the placement decision is made:

- PR title and number.
- Author login and current PR state.
- Description/body.
- Base and head branches.
- Additions, deletions, and changed-file statistics.
- The state, check-summary, and review-attention treatment from task 2.

The first slice should reuse the existing metadata response and avoid a second per-PR metadata request. Rendering, truncation, and placement remain intentionally undecided; this document is discovery material rather than an implementation contract.

## Discovery notes

Likely areas to inspect when this draft is promoted:

- `crates/gh/src/lib.rs`: `PrMeta` and `fetch_meta`, including the existing `gh pr view --json` field selection and fixtures.
- `crates/app/src/main.rs`: `Loaded`, `fetch_item`, `pr_titlebar_content`, the diff layout, and `pr_chat_header`/`chat_prompt`.
- `docs/tasks/task-2.md`: proposed shared state/check/review mapping and its three display surfaces.

These are pointers for the next planning pass, not settled ownership or API contracts.

## Open decisions

- Placement and collapse: should the overview sit above the diff, beside it, or adapt by window width? Is it expanded by default, and how is it collapsed or reopened?
- Markdown rendering: which Markdown constructs, links, code blocks, line breaks, and unsafe/external content rules should be supported? Can an existing renderer be reused?
- Empty or very long body: what placeholder represents no description, and should long content be clamped, truncated, scrollable, or expanded on demand?
- Avatars: should author avatars be included in this slice or deferred? If included, what are the image loading, caching, fallback, and error behaviors?

## Dependencies

- [Task 2: Show PR state, CI checks, and review attention](task-2.md) for the shared state/check/review mapping.
- Existing `PrMeta` fetching and the current PR review layout.

## Non-goals

- Editing the PR description.
- Building a full event timeline.
- Adding a commits view.
- Changing the review lifecycle or review submission behavior.

No acceptance criteria or test plan is prescribed yet; add them only after the open decisions are answered and this draft is promoted.
