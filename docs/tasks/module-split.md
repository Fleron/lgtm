# Module split of `crates/app/src/main.rs`

## What

Split the 11,085-line `main.rs` into domain modules with zero behaviour
change. Step 1 of 3 toward a second top-level view (issue/task tracker):

1. this split
2. top-level view switch with an empty tracker pane (`docs/tasks/view-switch.md`)
3. the tracker view itself

## Why

All UI lives in one file: one `ReviewApp` struct, one `impl` block with ~110
methods, one `Render`, one flat `mod tests` with 73 tests. A tracker view
cannot land there. Guiding constraint: keep the split easy and the module and
crate dependencies sane. Simplicity beats granularity.

## Decided (planning, 2026-09-12)

- Organise by feature/domain, flat under `src/`. A `review/` namespace can
  come with the view-switch task if it turns out to be needed.
- Cross-module field access via `pub(crate)` on fields other modules read.
- Tests co-located per module in `#[cfg(test)] mod tests`. Shared fixtures
  (`sample_diff`, `hunk`, `add`, `rem`, `ctx`, `cell`, `style`) move to a
  `#[cfg(test)] mod test_util` in `main.rs`.
- One commit per module move, each leaving `cargo test` green. Lands on main
  as one push when complete.
- Two-layer rule: a file either names `ReviewApp` (app layer) or it doesn't
  (pure layer). Pure files hold types and free fns, may use gpui element
  types, never `ReviewApp`, `Context<ReviewApp>` or `Entity<ReviewApp>`.
- Sidebar section renderers live in `sidebar.rs` (they take listeners), pure
  data for them (tree building, subscription persistence, cached-PR scan)
  lives in pure modules.
- `actions!` and `bind_keys` stay in `main.rs`; the `on_action` chain stays
  with `Render` in `app.rs`.
- No workspace crate changes.

## Target layout

Line refs are into today's `main.rs`. Sizes are estimates.

### Pure layer

| File | Moves | ~Lines |
|---|---|---|
| `diff/mod.rs` | `LineKind`, `ViewMode`, `Cell`, `Row` (278–368), `is_comment_row`, `nth_noncomment_row`, `build_rows` (1000), `kind_style`, `line_content`, `CardEdge`, gutter consts (77–79), `text_size`/`row_height*` (41–76) | 550 |
| `diff/syntax.rs` | `hunk_syntax` (795), `merge_highlights` (1331), `MAX_*_BYTES` (783–788) | 200 |
| `diff/gaps.rs` | `FileUpgrade` (864), `gap_span`, `push_gap_rows` (894–999), upgrade pipeline 2739–2892 | 300 |
| `comments.rs` | 396–522, 523–685, `comment_row` (1417), `comment_anchor` (4327) | 450 |
| `selection.rs` | 686–782, `SelectionInfo` + `selection_info` (3801–3877) | 200 |
| `minimap.rs` | 1828–2088 | 260 |
| `tree.rs` | 2089–2229 (`TreeEntry`, `build_tree`, `visible_entries`, `fuzzy_file_matches`, `status_style`, `TreeListRow`) | 150 |
| `subscriptions.rs` | 3324–3411 | 90 |
| `cached_prs.rs` | 3995–4132, `worktrees_root` (3982), `git_env` (134) | 170 |
| `theme.rs` | unchanged | 182 |

### App layer

| File | Moves | ~Lines |
|---|---|---|
| `main.rs` | `mod` decls, font consts, `actions!` (81–114), `main()` incl. keymap (145–277), `test_util` | 300 |
| `app.rs` | `struct ReviewApp` (4272), `new`, `zoom`, `char_width`, `active_item/data/_mut`, `pane_hit`, `pane_text_hit`, `render_footer`, `render_plus`, `centered_message`, `app_title`, `impl Render` (8583–8956) minus pane construction | 600 |
| `diff/render.rs` | `render_row` (1497–1827), new `render_pane` (pane construction from `Render`, 8587–8860), `expand_gap`, `toggle_view`, `resync_comment_wrap`, `jump*`, `toggle_comments`, `minimap_scrub_to`, `render_minimap` (7437) | 900 |
| `items.rs` | 2337–2738 (`Source`, `ItemState`, `ItemData`, `ReviewItem`, `fetch_item`), `open_item`, `spawn_fetch`, `spawn_upgrade`, `submit_open`, `activate`, `close_item`, `cycle_items`, `refresh`, `refetch_meta`, `refetch_comments` | 800 |
| `titlebar.rs` | 2932–3149, `render_titlebar` (7528) | 260 |
| `sidebar.rs` | `render_sidebar` (7568–8030) as shell + one fn per section, `render_tree_row` (2230), `SIDEBAR_MAX_LIST_HEIGHT`, `jump_to_file`, `tree_entry_clicked`, `tree_filter_confirm`, `refresh_cached_prs`, `start_subscribed_pr_poll`, `refresh_subscribed_prs`, `subscribe_repo`, `open_cached_pr`, `open_subscribed_pr`, `delete_cached_pr` | 750 |
| `palette.rs` | 3150–3323, 5101–5503, `render_palette` (8031) | 950 |
| `composer.rs` | 3412–3593, 3731–3800, composer/review methods 6115–6510, `render_composer`, `render_review` | 850 |
| `chat.rs` | 3594–3730, 3878–3916, `ExplorePlan` (4133, verify), 6539–7078 | 800 |
| `lsp.rs` | 3917–3981, 4148–4271, LSP/hover/nav methods 4913–6114, `render_lsp_status`, `render_hover`, `render_source_view` | 900 |
| `lsp_client.rs` | unchanged | 1226 |

Dependency direction: `main.rs` → `app.rs` → other app-layer files → pure
files → workspace crates. App-layer files all extend `ReviewApp` and may
reference each other. Pure files may reference other pure files.

## The one non-move edit

`impl Render` builds the diff pane inline (~270 lines computing `pane`). That
block becomes `fn render_pane(&mut self, window, cx)` in `diff/render.rs`,
body copied verbatim, and `Render` calls it. Everything else is a pure move
plus `use`/`pub(crate)` edits.

## Commit sequence

Leaves first, so each commit only adds `use` lines to `main.rs`.

1. `test_util` + `comments.rs`, `selection.rs`
2. `minimap.rs`, `tree.rs`
3. `subscriptions.rs`, `cached_prs.rs`
4. `diff/mod.rs`, `diff/syntax.rs`, `diff/gaps.rs`
5. `items.rs`
6. `titlebar.rs`, `palette.rs`
7. `composer.rs`, `chat.rs`
8. `lsp.rs`
9. `sidebar.rs` (incl. section fn extraction)
10. `diff/render.rs` (incl. `render_pane` extraction)
11. `app.rs`; `main.rs` is bootstrap only

## Verification

Per commit:
- `cargo build -p lgtm` with no new warnings.
- `cargo test -p lgtm`: 73 tests pass, count unchanged.

At the end:
- `git diff --color-moved=dimmed-zebra main..HEAD -- crates/app/src` shows
  only moved blocks plus `use`, `mod`, `pub(crate)` lines and the
  `render_pane` signature.
- Layering: `grep -L ReviewApp` over the pure files lists all of them.
- No file over ~1,000 lines except `lsp_client.rs`.
- Smoke run: open a PR, open a local folder, cmd-k, cmd-j chat, hover/F12,
  composer, minimap scrub.
- Clean subagent code review with no major findings.
