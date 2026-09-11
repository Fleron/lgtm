# Review-required and CI check status in the titlebar

## What

1. Render a "review required" tag in the open-PR titlebar (GitHub's
   `REVIEW_REQUIRED` decision is fetched but currently dropped silently).
2. Fetch and render a CI checks pass count (e.g. "4/4"), which isn't fetched
   anywhere today.

## Why

GitHub's own PR list surfaces both at a glance: whether review is still
required, and whether CI is green. lgtm's titlebar already shows the review
decision for approved/changes-requested PRs but goes blank for PRs still
awaiting review, and never shows CI status at all, so reviewers have to
leave the app to check either.

## How

**Review required (one match arm).** `gh::PrMeta.review_decision`
(`crates/gh/src/lib.rs:110-113`) is already fetched via `reviewDecision`, and
its own doc comment says GitHub can return `"REVIEW_REQUIRED"`. But
`pr_titlebar_content`'s match (`main.rs:2931-2935`) only handles `"APPROVED"`
and `"CHANGES_REQUESTED"`; everything else, including `"REVIEW_REQUIRED"`,
falls into `_ => None` and renders nothing.

Step: add a `"REVIEW_REQUIRED" => Some((theme::peach(), "review required"))`
arm (`theme.rs` has no yellow; `peach()` is the closest warm color already
in the palette) to that match.

**CI checks (new fetch + field + render).** No `statusCheckRollup` or
check-run data is fetched anywhere in `crates/gh` today. `gh pr view --json`
already supports a `statusCheckRollup` field, so this is one more name on
the existing `--json` string, not a new API call or a hand-written GraphQL
query.

Steps:
1. `gh/lib.rs:129-130`: append `,statusCheckRollup` to `fetch_meta`'s
   `--json` string.
2. `gh/lib.rs`: add a minimal struct (just `state`/`conclusion` per check) to
   deserialize the rollup, and `pub status_check_rollup: Vec<CheckRun>` (or
   similar) to `PrMeta`.
3. `main.rs` `pr_titlebar_content`: derive a `passed/total` count from
   `meta.status_check_rollup` and render it as a second `Tag::custom`, colored
   green when `passed == total` and red otherwise, next to the review-decision
   tag.

Effort: small. Same `gh pr view` call, no new dependencies. The list/picker
view could get the same two fields later as a separate, independent change if
wanted there too (same pattern, applied to `PrSummary`/`list_prs`).
