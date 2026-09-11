# Show PR state, CI checks, and review attention

## Goal

Make each PR's state, aggregate checks, and review attention easy to scan consistently in the active title bar, command-palette PR picker, and subscribed-PR sidebar. Use the existing `gh pr list`/`gh pr status` field semantics and GitHub's [status-check documentation](https://docs.github.com/en/pull-requests/reference/status-checks) as references; do not fetch each PR's individual reviews or checks.

## Scope

- Extend the existing `gh pr list --json` invocation in `crates/gh/src/lib.rs::list_prs` with `reviewDecision,statusCheckRollup`, while retaining `isDraft` as an optional/default-safe field too.
- Make `PrSummary.isDraft`, `PrSummary.reviewDecision`, and `PrSummary.statusCheckRollup` each optional/default-safe (`#[serde(default)]` or an equivalent representation); missing, `null`, empty, mixed, or unrecognized rollup data from a supported `gh` output must not make an entire PR list fail to deserialize.
- Add optional/default-safe `isDraft` to `PrMeta` and request it from `fetch_meta` so the active title bar can distinguish drafts from open PRs.
- Provide the active title bar with the same aggregate checks data: include a default-safe `statusCheckRollup` field in `PrMeta` and request it in the existing `gh pr view` call.
- Centralize state, check-summary, and review-attention mapping, then use that mapping in the active title bar, command-palette PR picker, and subscribed-PR sidebar.
- Keep the existing refresh/subscription flow and compact rows; do not add per-PR fetches or a second polling path.

The relevant CLI references are the official [`gh pr list`](https://cli.github.com/manual/gh_pr_list) and [`gh pr status`](https://cli.github.com/manual/gh_pr_status) manuals.

## Display semantics

### PR state

- For an active draft PR, show a gray state glyph and the literal `Draft` in the title bar in place of the current `Open` label. Do not show the green open-state treatment.
- Use the same gray draft treatment and a compact draft indicator/tooltip in the palette and subscribed sidebar where space permits. Only the active title bar is required to replace `Open` with the literal `Draft`; compact list surfaces may abbreviate it. Preserve existing state handling outside this draft treatment.
- A draft suppresses the `REVIEW_REQUIRED` warning, but does not suppress check status. Keep `CHANGES_REQUESTED` visibly distinct when present because it represents an actionable requested change.

### Checks

Show a GitHub-like checks glyph and `successful/total` progress when the rollup is present and classifiable. Count `SKIPPED` checks as successful, as GitHub does.

- All entries classified successful (`SUCCESS` or `SKIPPED` conclusions, or `SUCCESS` status contexts): green glyph and full progress.
- Any failure state (`FAILURE`, `ERROR`, `STARTUP_FAILURE`, `CANCELLED`, `TIMED_OUT`, `STALE`, or `ACTION_REQUIRED`): failure glyph/treatment and the successful/total progress.
- Any queued, requested, in-progress, or pending entry with no failure: pending glyph/treatment, distinct from both success and failure, and progress.
- A `NEUTRAL`/unrecognized entry, malformed rollup, absent rollup, or empty rollup: neutral/unknown treatment; do not infer success or attention from it. Do not fabricate a count for missing/invalid data.
- When states are mixed, failure takes precedence over pending; pending takes precedence over all-success; unknown data prevents a green all-success result.

### Review attention

`reviewDecision` is GitHub's policy-level aggregate approval: `APPROVED` means the repository's configured approval requirements are satisfied. It is not merely evidence that any individual reviewer submitted an approval, and it is the signal for “does not need my attention” in the same sense as GitHub's waiting-for-approval view.

- `REVIEW_REQUIRED`: separate orange attention/one-way-style icon with a tooltip explaining that review is required. Suppress this warning for drafts.
- `CHANGES_REQUESTED`: distinct attention treatment (for example, a red changes/requested icon and tooltip), visually different from `REVIEW_REQUIRED`.
- `APPROVED`: no warning; if a review glyph is shown, use a quiet green check.
- Missing, empty, or unrecognized value: neutral and no inferred warning.

### Surfaces

The active title bar, command-palette picker, and subscribed sidebar must show the same semantic state, check result, and review-attention colors/icons. The title bar may use fuller labels and tooltips; compact rows may abbreviate text, but must not change the mapping or attention meaning.

## Performance and refresh behavior

`statusCheckRollup` increases the existing `gh pr list`/`gh pr view` payload in proportion to the number of check contexts. Accept that bounded payload growth, decode only the minimal status/conclusion/state fields needed for aggregation, and avoid retaining/rendering verbose per-check details. A subscribed refresh remains one existing list call per repository; there must be no N+1 check/review requests, per-PR fetch loop, or additional polling interval.

## Affected areas

- `crates/gh/src/lib.rs`: `PrMeta`, `PrSummary`, `fetch_meta`, `list_prs`, a minimal default-safe status-rollup representation/normalizer, CLI-version/unsupported-field error handling, and PR view/list JSON fixtures.
- `crates/app/src/main.rs`: `pr_titlebar_content`, `palette_pr_row`, subscribed-PR rendering in `render_sidebar`, one shared pure display-mapping helper, and existing `PrMeta`/`PrSummary` test fixtures.

## Acceptance criteria

- The existing list request asks for `isDraft`, `reviewDecision`, and `statusCheckRollup`; the existing view request supplies `isDraft`, `reviewDecision`, and `statusCheckRollup` for the active title bar.
- Missing/null/empty/unrecognized fields from a supported `gh` output deserialize safely as neutral/unknown without dropping the PR list or active item. If the installed `gh` CLI is too old to accept the requested fields, surface a clear actionable error naming the minimum supported version and upgrade path; do not silently reinterpret an unsupported-field error as neutral data.
- One centralized mapping drives all three surfaces, with no surface-specific interpretation of draft, checks, or review decisions.
- Active drafts show a gray glyph and literal `Draft` instead of `Open`; `REVIEW_REQUIRED` is not shown as a warning for drafts, while check status remains visible and `CHANGES_REQUESTED` remains distinct.
- Checks display GitHub-like glyphs and successful/total progress, with green all-success, distinct pending, failure for every listed failure state, skipped counted as successful, and neutral treatment for none/unknown.
- Review-required and changes-requested attention states are visually distinct; approved and unknown states do not produce an inferred warning.
- Refreshing subscribed repositories adds no new command, polling loop, N+1 behavior, per-PR fetch, or individual-review request; only the existing responses become richer.
- The payload-growth tradeoff is covered in code comments or tests, and rendering remains aggregate-only rather than a per-check detail list.

## Tests

- Add/adjust `gh` fixtures for `PrMeta` and `PrSummary` with `isDraft`, `reviewDecision`, CheckRun-style `status`/`conclusion`, StatusContext-style `state`, skipped, pending, each listed failure state, unknown entries, empty/null rollups, and omitted fields. Assert the list/view field selections include the required names where request testing is available.
- Add app-level pure mapping tests for open/draft state, all-success counts, skipped counts, pending precedence, failure precedence, unknown/none, review-required draft suppression, changes-requested distinction, approved, and unknown review decisions.
- Manually inspect all three surfaces using real PRs representing open, draft, all-success, pending, failed, changes-requested, review-required, approved, and missing/empty status data. Confirm tooltips explain orange review attention and the check aggregate without extra detail calls.
- Manually refresh a subscribed repository and verify one existing list request supplies the aggregate fields, with no per-PR review/check requests or visible UI stalls.
- Verify with:

  - `cargo fmt --check`
  - `cargo test -p gh`
  - `cargo test -p lgtm`
  - `cargo test --workspace`

## Non-goals

- Adding a new PR filter workflow.
- Tracking whether the current user personally reviewed or approved a PR.
- Fetching individual reviews, check runs, or detailed check output.
- Changing mergeability, branch-protection policy, or review submission behavior.
