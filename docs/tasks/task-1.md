# Delete local review draft comments

## Goal

Let a reviewer remove a comment that exists only in the current local review, including comments in a reply thread, before copying or otherwise using the local review. Keep this entirely in-memory and local to the active local review.

## Scope

- Add a visible delete affordance to each local draft comment row, for both thread roots and replies.
- Deleting a root removes the root and all of its descendants. Deleting a reply removes that reply and any descendants it owns, while retaining the rest of the thread.
- Make deletion a safe no-op for an unknown or already-removed comment ID.
- After deletion, rebuild the local `CommentIndex` and display rows from the remaining comments. Preserve the current cursor/viewport anchor using the existing anchored row-rebuild machinery.
- Ensure removed comments no longer appear in the generated `local_review_prompt` or the copied local-review prompt.

## Affected areas

- `crates/app/src/main.rs`: `LocalReview`, `LocalReview::add_comment`, `local_review_threads`, `local_review_prompt`, `add_local_comment`, comment-row/action rendering, and the existing `ItemData` row/index rebuild path.
- No GitHub client or network code should be needed.

## Acceptance criteria

- A local root and a local reply each expose a discoverable delete action that targets the correct comment.
- Removing a root removes its complete reply tree; removing a reply does not remove its siblings or parent.
- The diff comment index, rendered rows, and thread/card layout immediately reflect the remaining comments.
- The current viewport/cursor remains as stable as the existing anchored rebuild supports, including when the deleted row is visible.
- The deleted body and location are absent from the generated local-review prompt.
- No `gh` process, HTTP request, or PR state is touched by this feature.
- Existing open-composer Cancel/Escape behavior is unchanged.

## Tests

- Add focused unit coverage for root deletion, reply/subtree deletion, unknown IDs, index rebuilding, and omission from `local_review_prompt`.
- Exercise the delete affordance for both a root and a reply. If UI automation helpers cannot cover either affordance, manually verify both and document the result.
- Verify with:

  - `cargo fmt --check`
  - `cargo test -p lgtm`
  - `cargo test --workspace`

## Non-goals

- Deleting PR comments, submitted GitHub comments, or GitHub pending reviews.
- Implementing GitHub review-draft lifecycle or persistence for local comments.
- Changing the existing composer Cancel/Escape flow or adding network fallbacks.
