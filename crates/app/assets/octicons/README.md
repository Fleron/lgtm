# Octicons for the tracker view

The 22 SVGs in this directory are extracted verbatim from the inline
`<symbol>` sprite in `docs/tasks/assets/tracker-mockup.html` (the accepted
visual, sketch v9) — that file is self-contained and already carries real
Primer Octicons path data, so these are not hand-guessed. Each file is
`viewBox="0 0 16 16"`, `fill="currentColor"`, named after the Primer
Octicons filename from `docs/tasks/tracker.md`'s Icons table.

Covered here: `bug-16`, `calendar-16`, `comment-16`, `filter-16`,
`flame-16`, `git-merge-16`, `git-pull-request-16`,
`git-pull-request-draft-16`, `issue-closed-16`, `issue-draft-16`,
`issue-opened-16`, `issue-tracked-by-16`, `issue-tracks-16`,
`link-external-16`, `list-ordered-16`, `milestone-16`, `pencil-16`,
`person-16`, `plus-16`, `screen-full-16`, `tag-16`, `x-16`. That's every
icon the doc's table lists — no gap.

`gpui_component::IconName` already covers a few of these with its own
(non-octicon) glyphs: `Calendar`, `Plus`, `Close` (x), `Maximize`
(screen-full), `User` (person), `ExternalLink` (link-external). Whether the
tracker UI uses those built-ins or the octicon files here for visual
consistency with the mockup is the UI implementer's call.

## Wiring gap

`main.rs` currently registers only `gpui_component_assets::Assets` via
`.with_assets(gpui_component_assets::Assets)`; nothing serves files from
`crates/app/assets/`. Loading these through `gpui::svg("octicons/<name>.svg")`
needs an `AssetSource` that also resolves this directory (wrapping or
chaining alongside `gpui_component_assets::Assets`) wired into that same
`.with_assets(...)` call. Left for whoever wires the tracker UI, since that
call sits in the shared `main.rs` App startup path.
