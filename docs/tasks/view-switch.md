# Top-level view switch: Review | Tracker

## What

Add a `TopView { Review, Tracker }` to lgtm so the user switches between the
existing PR-review screen and a tracker screen. This task delivers the switch
and the tracker's chrome only; the tracker content is `docs/tasks/tracker.md`.
Step 2 of 3, after `docs/tasks/module-split.md`.

## Why

lgtm is one screen today. A tracker view needs a place to live, a way to get
there and back, its own keymap context, and its own sidebar and main pane
without disturbing review state.

## Decided (planning, 2026-09-12)

- Switch via global keybindings `cmd-1` (Review) and `cmd-2` (Tracker), and a
  two-segment control at the left of the titlebar, before the app title.
- Shared chrome: titlebar and footer. Per-view: sidebar and main pane. The
  chat pane (`cmd-j`) stays review-only.
- Review state is untouched by switching. Items, scroll, selection, chat, LSP
  all persist. Switching back restores the exact prior screen.
- Flat modules: new `tracker.rs` beside the others. No `review/` nesting.
- Tracker pane gets its own `key_context("Tracker")` so review-only bindings
  (`]`, `n`, `v`, `c`, `r`, ...) do not fire there. Global bindings (`cmd-b`,
  `cmd-k`, `cmd-1/2`, zoom, `cmd-q`) work in both views.
- `cmd-t` (OpenInput) and `cmd-w` (CloseItem) are review-only; in Tracker they
  are no-ops until the tracker task defines them.
- Footer hints switch with the view (review hints today; tracker hints listed
  in `tracker.md`).
- Placeholder content for this task: tracker sidebar and main pane render
  `centered_message("Tracker")` so the switch is visibly working.

## How

- `app.rs`: `top_view: TopView` on `ReviewApp`, default `Review`.
  `Render` branches on it for sidebar and main pane. Titlebar right-side
  content branches too (PR/local meta for Review, nothing yet for Tracker).
- `main.rs`: `ShowReview`, `ShowTracker` actions; bindings `cmd-1`, `cmd-2`
  with `None` context.
- `titlebar.rs`: segmented control, two `Button`s from `gpui_component`,
  active one filled. Click calls the same handlers as the actions.
- `tracker.rs`: `render_tracker_sidebar`, `render_tracker_pane`, both
  placeholders, plus the `Tracker` key context on the pane.
- `app.rs` footer: `render_footer` takes the view and picks the hint set.

## Verification

- `cargo build -p lgtm` no new warnings; `cargo test -p lgtm` green.
- Smoke: open a PR, scroll mid-file, `cmd-2` shows the placeholder, `cmd-1`
  returns to the same scroll position and file. Repeat via the titlebar
  control. `]` in Tracker does nothing; `cmd-b` toggles the tracker sidebar.
- Clean subagent code review with no major findings.
