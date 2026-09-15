# PR state, CI checks, and review attention

Status: Partially implemented. The current source fetches the aggregate
fields and shows an initial review/CI indicator in the subscribed-PR and
open-items sidebar rows. The remaining work below is the implementation
contract for completing the feature; it deliberately does not move the
review/CI indicators back into the active content-pane titlebar.

The related draft-state titlebar work is implemented in
[draft-pr-label.md](draft-pr-label.md). The titlebar should continue to show
`Draft` or `Open` as its PR state, while review and CI attention remain
available where a reviewer scans the PR lists.

## Goal

Make each PR's draft state, aggregate checks, and review attention easy to
scan in all relevant PR-list surfaces: the subscribed-PR sidebar feed, the
open-items sidebar list, and the command-palette PR picker. Use GitHub's
policy-level review decision and aggregate status-check semantics; do not
fetch each PR's individual reviews or checks.

The intended meaning of review attention is the same as GitHub's waiting-for-
approval view: a PR with `REVIEW_REQUIRED` still needs an approval under the
repository's configured policy, regardless of whether the current user has
personally reviewed it.

## Current implementation

The merged source already includes the following partial implementation:

- `gh::fetch_meta` requests `isDraft`, `reviewDecision`, and
  `statusCheckRollup`.
- `gh::list_prs` requests `isDraft`, `reviewDecision`, and
  `statusCheckRollup` in the existing list call.
- `PrMeta` and `PrSummary` carry review decisions and a minimal check-run
  representation.
- Commit `b1af5ea` adds a gray `Draft` titlebar state, and commit `27d6393`
  carries that draft treatment into the sidebar dots.
- Commit `27d6393` adds a shared sidebar indicator to the subscribed-PR feed
  and open-items list, with an initial warning/check icon and passed/total
  count.

The current implementation is intentionally recorded as partial: its
indicator treats non-`REVIEW_REQUIRED` decisions alike, colors every
non-all-success check aggregate as failure, does not yet distinguish pending
or unknown data, does not suppress the review warning for drafts, and does
not yet cover the command-palette picker. The field models also need the
default/null-safe handling specified below.

## Scope

- Keep `isDraft`, `reviewDecision`, and `statusCheckRollup` in the existing
  `gh pr list` request. Keep the corresponding fields in the existing
  `gh pr view` request for the active titlebar's draft/open state and any
  shared data needed by the open-items row.
- Make `PrSummary.isDraft`, `PrSummary.reviewDecision`,
  `PrSummary.statusCheckRollup`, and the corresponding `PrMeta` fields
  default/null-safe. Missing, `null`, empty, mixed, or unrecognized rollup
  data from supported `gh` output must not make a PR list fail to deserialize.
- Decode only the minimal status/conclusion/state fields needed for the
  aggregate. Do not retain or render verbose per-check details.
- Centralize draft, check-summary, and review-attention mapping in one pure
  helper and use it in all three PR-list surfaces:
  subscribed-PR sidebar rows, open-items sidebar rows, and the command-
  palette PR picker.
- Keep the existing refresh/subscription flow and compact rows. Do not add a
  second polling path or per-PR fetches.

The relevant CLI references are the official [`gh pr list`](https://cli.github.com/manual/gh_pr_list)
manual and GitHub's [status-check documentation](https://docs.github.com/en/pull-requests/reference/status-checks).

## Display semantics

### Draft state

- An active draft PR shows a gray state glyph and the literal `Draft` in the
  titlebar in place of `Open`; it does not use the green open-state
  treatment. This behavior is covered by
  [draft-pr-label.md](draft-pr-label.md).
- The subscribed sidebar, open-items sidebar, and palette picker use the
  same gray draft treatment wherever their compact layout permits.
- A draft suppresses the `REVIEW_REQUIRED` warning in all list surfaces, but
  does not suppress CI status. `CHANGES_REQUESTED` remains visible and
  distinct because it represents an actionable requested change.

### CI checks

When the rollup is present and classifiable, show a GitHub-like checks glyph
and `successful/total` progress. Count `SKIPPED` checks as successful, as
GitHub does.

- All entries classified successful (`SUCCESS` or `SKIPPED` conclusions, or
  `SUCCESS` status contexts): green glyph/treatment and full progress.
- Any failure state (`FAILURE`, `ERROR`, `STARTUP_FAILURE`, `CANCELLED`,
  `TIMED_OUT`, `STALE`, or `ACTION_REQUIRED`): failure glyph/treatment and
  the successful/total progress.
- Any queued, requested, in-progress, or pending entry with no failure:
  pending glyph/treatment, distinct from both success and failure, and the
  progress.
- A `NEUTRAL`/unrecognized entry, malformed rollup, absent rollup, or empty
  rollup: neutral/unknown treatment. Do not infer success or attention and do
  not fabricate a count for missing/invalid data.
- When states are mixed, failure takes precedence over pending; pending
  takes precedence over all-success; unknown data prevents a green
  all-success result.

### Review attention

`reviewDecision` is GitHub's policy-level aggregate approval. `APPROVED`
means the configured approval requirements are satisfied; it is not merely
evidence that any individual reviewer submitted an approval.

- `REVIEW_REQUIRED`: separate orange attention icon (the product may use an
  orange one-way-style/"enkelriktat" treatment) with a tooltip explaining
  that review is required. Suppress this warning for drafts.
- `CHANGES_REQUESTED`: a distinct changes-requested treatment, such as a
  red icon and explanatory tooltip; it must not be conflated with
  `REVIEW_REQUIRED`.
- `APPROVED`: no warning; a quiet green check is allowed if a review glyph is
  shown.
- Missing, empty, or unrecognized values: neutral and no inferred warning.

The same semantic mapping and attention meaning must be used on all three
list surfaces. Compact rows may abbreviate labels, but must not change the
state mapping, colors, or warning distinction.

## Performance and refresh behavior

`statusCheckRollup` increases the existing `gh pr list`/`gh pr view` payload
in proportion to the number of check contexts. Accept that bounded payload
growth, decode only the minimal fields needed for aggregation, and avoid
retaining/rendering verbose check details.

A subscribed refresh remains one existing list call per repository. There
must be no N+1 check/review requests, per-PR fetch loop, individual-review
request, or additional polling interval. The feature should not introduce a
visible stall while a sidebar refresh is in progress.

If the installed `gh` CLI is too old to accept a requested field, surface a
clear actionable compatibility error naming the required upgrade path; do
not silently reinterpret an unsupported-field error as neutral data.

## Affected areas

- `crates/gh/src/lib.rs`: `PrMeta`, `PrSummary`, `fetch_meta`, `list_prs`, a
  minimal default-safe status-rollup representation/normalizer, CLI-version
  error handling, and list/view JSON fixtures.
- `crates/app/src/main.rs`: the existing titlebar draft state, the shared
  display-mapping helper, `palette_pr_row`, subscribed-PR rendering in
  `render_sidebar`, open-items sidebar rows, and `PrMeta`/`PrSummary` test
  fixtures.

## Acceptance criteria

- The existing list request asks for `isDraft`, `reviewDecision`, and
  `statusCheckRollup`; the existing view request supplies the draft/open
  fields and aggregate data without adding a new polling path.
- Missing/null/empty/unrecognized fields from supported `gh` output
  deserialize safely as neutral/unknown without dropping the PR list or
  active item. An unsupported-field error from an old CLI remains an
  actionable error.
- One centralized mapping drives the subscribed sidebar, open-items sidebar,
  and command-palette picker. No surface-specific interpretation of draft,
  checks, or review decisions is allowed.
- Active drafts show a gray glyph and literal `Draft` instead of `Open`; list
  surfaces use the same gray draft treatment. `REVIEW_REQUIRED` is hidden as
  a warning for drafts, while checks remain visible and
  `CHANGES_REQUESTED` remains distinct.
- Checks show GitHub-like glyphs and successful/total progress, with green
  all-success, distinct pending, failure for every listed failure state,
  skipped counted as successful, and neutral treatment for none/unknown.
- Review-required and changes-requested attention states are visually
  distinct; approved and unknown states do not produce an inferred warning.
- Refreshing subscribed repositories adds no new command, polling loop,
  N+1 behavior, per-PR fetch, or individual-review request. Payload growth
  and aggregate-only rendering are covered by comments or tests.

## Tests

- Add/adjust `gh` fixtures for `PrMeta` and `PrSummary` with `isDraft`,
  `reviewDecision`, CheckRun-style `status`/`conclusion`, StatusContext-style
  `state`, skipped, pending, each listed failure state, unknown entries,
  empty/null rollups, and omitted fields. Assert the list/view field
  selections include the required names where request testing is available.
- Add pure app mapping tests for open/draft state, all-success counts,
  skipped counts, pending precedence, failure precedence, unknown/none,
  review-required draft suppression, changes-requested distinction,
  approved, and unknown review decisions.
- Manually inspect all three surfaces with open, draft, all-success, pending,
  failed, changes-requested, review-required, approved, and missing/empty
  status data. Confirm tooltips explain orange review attention and the
  check aggregate without extra detail calls.
- Manually refresh a subscribed repository and verify one existing list
  request supplies the aggregate fields, with no per-PR review/check requests
  or visible UI stalls.
- Verify with:

  - `cargo fmt --check`
  - `cargo test -p gh`
  - `cargo test -p lgtm`
  - `cargo test --workspace`

## Non-goals

- Adding a new PR filter workflow.
- Tracking whether the current user personally reviewed or approved a PR.
- Fetching individual reviews, check runs, or detailed check output.
- Changing mergeability, branch protection, or review submission behavior.
- Moving review/CI indicators into the active titlebar; the titlebar draft
  state remains covered separately by `draft-pr-label.md`.
