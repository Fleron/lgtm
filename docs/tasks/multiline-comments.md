# Multi-line review comments

Status: Implemented in `b916363`.

## Delivered behavior

- A reviewer can drag-select multiple rows using the existing diff selection
  and click the comment `+` affordance at either end of that selection.
- When the selection is a valid same-side range in one file, the composer
  records the first and last line and displays the range in its target label.
- Local review comments preserve the range in their in-memory
  `ReviewComment` data.
- PR comments send GitHub's `start_line` and same-side `start_side` fields in
  the existing review-comment request, with the selected end line in the
  existing `line` field.
- Single-line comments and selections across different sides continue to
  use the existing single-line behavior.

The implementation reuses the existing selection, comment-anchor, composer,
and GitHub comment-posting paths; it does not add a separate range-selection
model or request flow.
