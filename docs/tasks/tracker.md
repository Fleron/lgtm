# Tracker view

## What

A GitHub-Projects-backed issue tracker as lgtm's second top-level view. Three
columns: a pick-next queue on the left as a Taskwarrior table, everything in
flight in the middle grouped by status and sorted by urgency, and a collapsible
issue panel on the right with full editability. Step 3 of 3, after
`docs/tasks/view-switch.md`.

Accepted visual (sketch v9, 2026-09-13), three states: panel closed, epic
open, sub-issue open. Two copies, same content:

- Published artifact: https://claude.ai/code/artifact/5b3d9e5d-e331-4e7a-9c0c-3a95ab211ed5
  (a Claude Code session reads it with the Artifact tool, `action: read`).
- Source in repo: `docs/tasks/assets/tracker-mockup.html`. Open it in a
  browser; it is self-contained (inline CSS, octicon SVG sprite, no network).

If the two ever differ, the repo copy wins.

## Why

Emil wants to plan and take work from the same tool he reviews in. The left
column answers "what do I pick next", the middle answers "what am I doing and
what is about to be late", the right lets him change anything without leaving
lgtm. Urgency is the most important number on the screen.

## Layout

Shared titlebar and footer from the view switch. Body is two columns, three
when an issue is open.

### Titlebar (Tracker view)

- `Review | Tracker` segmented control, repo name, counts per group
  (`4 doing · 2 in review · 1 blocked · 11 ready · 20 later`).
- Right side: assignee filter chip, text only (`me`, `me + unassigned`,
  `all`), in the slot the LSP chip uses in Review. Click or `a` cycles.

### Left column, sidebar (~1/3 width)

- Section `Milestones` with the milestone octicon on the title only. Chips
  are plain text with `done/total`, multi-select. Nothing selected = all.
  Closed milestones are not shown.
- Section `Next up` with the count of ready issues, then a filter box.
- Taskwarrior table, sorted by urgency desc. Columns in order:
  `ID`, type icon, `Title` (clipped to column like the review sidebar),
  tags (comma-separated text, clipped), `Age`, priority (`H`/`M`/`L`),
  milestone (short name), due (relative countdown), urgency. Column headers
  for tags, priority, milestone, due and urgency are octicons (tag,
  list-ordered, milestone, calendar, flame).
- Below the table a collapsed divider `▸ todo 8 · backlog 12` for the other
  queue statuses.

### Middle column, main pane

- One group per `flight` status, in board order. Header is the status name
  in text only, on a band tinted with the group colour and a left border.
  No icon.
- Each group is a table sorted by urgency desc. Columns: `ID`, type icon,
  `Title`, tags (comma-separated, clipped), sub-issue progress (bar +
  `done/total`), linked PR (octicon coloured open/merged/draft + number,
  click opens github.com), assignee, urgency. Headers for tags, sub-issues,
  PR, assignee and urgency are octicons.
- Row colour, same rules in both columns: overdue red, due < 24h yellow,
  blocked italic grey, selected row green tint with left bar.

### Right column, issue panel (390px, collapsible like the chat pane)

- Header: type icon, `#id Title`, then external-link, expand and close
  glyphs. When the issue is a sub-issue, a breadcrumb `⟲ #parent ›` (the
  issue-tracked-by octicon) precedes the title; click or `⌫` goes back.
- Status chip, alone on its row. Click changes status.
- Metadata line, icon-only labels, in this order: calendar `date · countdown`
  (red when overdue), milestone, person `assignee`, list-ordered `priority`,
  flame `urgency` (number only), PR glyph `#n` when linked.
- Tag row: tag octicon then each label as a small coloured pill.
- `Description` with a pencil glyph; edits in place, markdown.
- `Sub-issues` section (parent issues only): issue-tracks octicon, progress
  bar + `done/total`, plus glyph. One row per child: a single state icon
  (issue-draft for backlog/todo, issue-opened green for ready, blue when in
  flight, issue-closed for done), `#id`, title. No type icon, no status
  text.
  Editing: `j`/`k` move, `↵` drills in, `space` opens a status menu on the
  row, `e` edits the title inline, `+` adds a new row that `↵` creates on
  GitHub and links to the parent.
- A sub-issue's panel has no sub-issues section. Everything else is the
  same. (GitHub allows nested sub-issues; deliberately not supported here.)
- `Activity` list, then a comment box (`⌘↵` to post).

### Footer (Tracker view)

`⌘1 review · ⌘2 tracker · j/k move · ⇥ column · ↵ open · s start ·
r review · d done · n new · / filter · a assignee · o github · ⌘K palette`.

## Icons

Octicons only, from primer.style/octicons. Icons replace a word; never both.

| Meaning | Octicon | Colour |
|---|---|---|
| type Bug | `bug-16` | red |
| type Feature | `issue-opened-16` | blue |
| type Task | `issue-opened-16` | peach (orange) |
| no type | `issue-opened-16` | green |
| type Epic | `issue-opened-16` | mauve (Emil to confirm) |
| sub-issue backlog/todo | `issue-draft-16` | overlay |
| sub-issue ready | `issue-opened-16` | green |
| sub-issue in flight | `issue-opened-16` | blue |
| sub-issue done | `issue-closed-16` | mauve |
| has sub-issues | `issue-tracks-16` | |
| is a sub-issue of | `issue-tracked-by-16` | |
| PR open / merged / draft | `git-pull-request-16` / `git-merge-16` / `git-pull-request-draft-16` | green / mauve / overlay |
| due | `calendar-16` | |
| milestone | `milestone-16` | |
| assignee | `person-16` | |
| priority | `list-ordered-16` | |
| urgency | `flame-16` | |
| labels | `tag-16` | |
| filter, edit, comment, add, close, expand, open on web | `filter-16`, `pencil-16`, `comment-16`, `plus-16`, `x-16`, `screen-full-16`, `link-external-16` | |

Type name → colour mapping lives in `tracker.json` so custom org types can
be added.

## Decided (planning, 2026-09-12/13)

- Data source: GitHub Projects v2 through the `gh` CLI. Status, Due and
  Priority are project fields (`gh project field-list` for options and
  order, `gh project item-list --format json` for values). Type, milestone,
  labels, sub-issues, linked PRs, comments, assignees come from the issue.
  `gh api graphql` where the `project` subcommands fall short (sub-issues,
  field mutations, blocked-by relations).
- Board: one per repo, auto-detected as the first Projects v2 board linked
  to the repo. Repo chosen from lgtm's subscribed-repo list; one repo at a
  time.
- Status → column mapping via `~/.cache/lgtm/tracker.json` (same ad-hoc JSON
  pattern as `subscriptions.json`), keyed by Status option name to `queue` /
  `flight` / `hidden`. Seeded on first open: Backlog, Todo, Ready → queue;
  Done, Merged → hidden; everything else → flight. Unknown options → flight.
  Items with no Status → queue.
- Assignee filter persisted in `tracker.json`.
- Urgency computed locally, Taskwarrior coefficients mapped to GitHub:

  | term | coefficient | source |
  |---|---|---|
  | due | 12.0 | Due field; 0.2 at 14d out, linear to 1.0 at due, 1.0 after |
  | blocking | 8.0 | open issues have a "blocked by" relation pointing here |
  | priority | 6 / 3.9 / 1.8 | Priority field High / Medium / Low |
  | active | 4.0 | status mapped to flight |
  | age | 2.0 | created-at, linear to 1.0 at 365d |
  | milestone | 1.0 | has a milestone |
  | labels | 1.0 | has any label |
  | comments | 1.0 | has comments |
  | blocked | -5.0 | status Blocked, or an open "blocked by" relation |

  Taskwarrior's next, scheduled, waiting and recurring terms have no GitHub
  equivalent and are dropped. Coefficients editable in `tracker.json`; the
  function must be monotonic in each input.
- Keyboard (Tracker context): `j`/`k` move, `tab` next column, `↵` open
  panel or drill into sub-issue, `⌫` back up the breadcrumb, `s` first
  flight status, `r` the status named "In review" if present, `d` first
  hidden status, `n` new issue, `/` focus filter, `a` cycle assignee filter,
  `o` open selected issue or PR on github.com, `escape` close panel.
- All writes go through `gh`; the UI applies optimistically and reverts
  with an inline red error on failure (same treatment as chat errors).
- Creating an issue (`n`) or sub-issue (`+`) uses the composer input: title
  required, then status, type, milestone, priority, labels. Sub-issues get
  the parent preselected. Saved via `gh issue create`, project add, and the
  sub-issue link mutation.
- Refresh on view enter, on `r`-style explicit refresh, and after every
  write. No background polling in this task.
- Out of scope: cross-repo boards, multiple boards per repo, nested
  sub-issues, drag and drop, Projects views/iterations, assignee editing
  beyond self-assign.

## How

- `tracker.rs` (app layer): `TrackerState` on `ReviewApp` (board id, field
  ids, items, config, selection per column, panel stack for breadcrumb),
  the three render fns, key handlers, and the `gh` calls via
  `cx.background_spawn`.
- `crates/gh`: new fns `list_project_boards(owner, repo)`,
  `project_fields(owner, number)`, `project_items(owner, number)`,
  `set_project_field(item, field, option)`, `issue_detail(number)` with
  sub-issues, labels, linked PRs and comments, `add_sub_issue`,
  `create_issue`, `post_comment`. Same `fn gh(args)` shellout pattern.
- Urgency, due formatting and status mapping as pure fns in a pure-layer
  `urgency.rs` with unit tests for the edge cases (no due, overdue, blocked,
  zero priority, unknown status).
- Icons: octicon SVG paths embedded as `gpui::svg` assets under
  `crates/app/assets/octicons/`, loaded through the existing
  `gpui-component-assets` path.
- Reuse: `uniform_list` and filter input from the sidebar, `Tag`/`Button`
  from `gpui_component`, `centered_message`, `short_age`, composer input,
  chat error rendering.

## Verification

- `cargo build -p lgtm` no new warnings; `cargo test -p lgtm` green, plus
  urgency and status-mapping unit tests.
- Smoke against a real board on the lgtm repo: queue sorted by urgency with
  Ready before the collapsed todo/backlog, milestone chips filter both
  columns, assignee chip cycles and filters, `s` on a queue row moves it to
  the middle and to the board (check on github.com), overdue item shows red
  negative countdown, opening an epic shows the sub-issue list with progress
  bar, `↵` on a child shows the breadcrumb, `⌫` returns, description edit
  round-trips, sub-issue add appears under the parent on GitHub, `o` opens
  the PR.
- Kill the network and confirm writes revert with inline red error.
- Clean subagent code review with no major findings.
