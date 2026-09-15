# Draft PR label in the titlebar

Status: Implemented in `b1af5ea` and completed for sidebar state treatment
in `27d6393`.

## Delivered behavior

- `gh::fetch_meta` requests `isDraft`, and `gh::PrMeta` carries the value for
  the active PR.
- An active draft PR shows a gray `Draft` state in the titlebar instead of
  the green `Open` state.
- Draft PR dots in the subscribed-PR feed and open-items sidebar are gray as
  well, so draft state remains visible before and after opening a PR.
- Non-draft open, merged, and closed PR state treatments remain unchanged.

The implementation reuses the existing `theme::overlay0()` gray and the
existing state/dot rendering paths; no separate draft-status request or
polling path was added.
