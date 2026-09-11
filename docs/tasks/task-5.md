# Draft: PR conversations

Status: Draft / not implementation-ready. The open product decisions below must be resolved before this becomes an executable implementation task.

## Goal

Explore a conversation section for a PR so reviewers can read general discussion and submitted review summaries alongside the diff. Preserve the existing inline discussion experience and make the first slice useful without turning it into a full timeline.

## Current support

- `gh::fetch_review_comments` fetches inline pull-request review comments and replies from the REST pulls/comments endpoint.
- `group_comments` in `crates/app/src/main.rs` groups those comments into threads, and the resulting rows are rendered at their diff anchors.
- Top-level issue/PR comments are not currently fetched or rendered.
- Submitted review summaries and their verdicts are not currently fetched or rendered, even though review submission exists separately.

## Proposed first slice

After the PR summary from [task 4](task-4.md), add a Conversation section that can show:

- General PR/issue comments with author, time, and body.
- Submitted review summaries with author, time, body, and verdict.
- Optionally, an index of inline threads with a jump-to-diff action, while keeping the full inline thread at its existing anchor.

Fetch the new conversation data concurrently with, or lazily after, the diff is usable so the diff remains responsive. Use bounded pagination/loading and one aggregate conversation fetch where possible; do not add N+1 requests per comment or a per-comment polling loop. These are candidate behaviors pending the decisions below, not implementation requirements yet.

## Discovery notes

Likely areas and API surfaces to inspect when this draft is promoted:

- `crates/gh/src/lib.rs`: `ReviewComment`, `fetch_review_comments`, the existing REST `pulls/{number}/comments` call, and the GitHub/`gh` API shape for general comments and submitted reviews.
- `crates/app/src/main.rs`: `fetch_item`, `CommentIndex`, `group_comments`, inline comment row rendering, and the PR layout where the task-4 summary would live.
- Existing Markdown, avatar, loading, and error components, if any, should be identified before choosing new rendering behavior.

These pointers are discovery notes only; endpoint choice, models, ownership, and layout remain open.

## Open decisions

- Ordering: should general comments and review summaries be merged into one chronological stream, or shown as grouped sections?
- Inline threads: should the Conversation section include inline threads, only an index with jump-to-diff links, or neither?
- Pagination and loading: what page size, load-more behavior, initial loading state, retry behavior, and partial-error treatment are appropriate?
- Rendering reuse: should conversation bodies reuse the overview's Markdown rules and any existing avatar component, including their caching/loading/error behavior?
- Visibility: should the section be collapsed or expanded by default, and should each comment/review be independently collapsible?

## Dependencies

- [Task 4: PR overview and summary](task-4.md) for the surrounding overview layout and placement decisions.
- Existing inline comment grouping and anchored diff rendering.

## Non-goals

- Showing commits or building a full event timeline.
- Rendering or managing pending-review drafts.
- Deleting PR comments.
- Adding multi-line commenting.

No acceptance criteria or test plan is prescribed yet; add them only after the open decisions are answered and this draft is promoted.
