# Dispatch to a remote devbox
Extends: 2026-09-15-dispatch-an-agent-from-the-issue-tracker.md

## Behaviours
R1. WHEN the dispatch card opens and at least one ssh host was discovered, THE card SHALL show a target chip after the agent chip, reading "Local" on open; clicking it SHALL open a `PopupMenu` listing "Local" and every discovered host, and picking an entry SHALL set the chip label and the card's target. No hosts discovered: chip hidden.
R2. WHEN the card opens, THE app SHALL read `~/.ssh/config` once and collect the names on `Host` lines (space-separated, several per line allowed, indentation ignored), skipping any name containing `*`, `?` or `!`. `Include` files are not followed.
R3. WHEN the user dispatches with a remote target, THE app SHALL run synchronously `ssh -o BatchMode=yes -o ConnectTimeout=10 <host> <script>` where `<script>` (run by the remote login shell) resolves the checkout dir, then runs `tmux new-window -t <agent>: -c <dir> -n '#<N>' "bash -lc '<agent> \"$(printf %s <b64> | base64 -d)\"'"`, falling back to `tmux new-session -d -s <agent> -c <dir> <same command>` when session `<agent>` does not exist. `<agent>` is `claude` or `codex`.
R4. THE checkout dir SHALL be `dispatch_remote_roots[<host>]/<repo>` when that optional config key has an entry for the host, otherwise the first existing directory among `~/<repo>` and `~/*/<repo>`. If none exists the script SHALL exit 3 and print "no checkout for <repo> under ~ on <host>".
R5. THE prompt SHALL be base64-encoded locally and decoded inside the remote command; a unit test pins the exact ssh argv for a prompt containing quotes, `$`, backticks and newlines.
R6. WHEN ssh exits 0, THE card SHALL close and focus return to the tracker pane; non-zero SHALL keep the card open and show stderr (or the exit-3 message) in red. The Local target keeps its existing behaviour and its `dispatch_root` requirement; a remote target does not require `dispatch_root`.
R7. THE `/` completion list stays local; no remote scan. No local window opens for a remote dispatch.

## Not doing
Attaching a Ghostty window, remembering the last target, per-target agent binaries, mosh, following ssh `Include`, syncing checkouts.

## Files
- crates/app/src/dispatch.rs — `Target` enum, `ssh_hosts(config_text)`, `remote_args(host, root_override, repo, agent, prompt)`, `dispatch_remote`, tests for R2/R5
- crates/app/src/urgency.rs — `dispatch_remote_roots: Option<BTreeMap<String, PathBuf>>`, skip_serializing_if none
- crates/app/src/tracker.rs — `target` + `hosts` on `DispatchCard`, `tracker_set_dispatch_target`, submit branching
- crates/app/src/tracker_panel.rs — target chip with PopupMenu

## Verification
`cargo test -p lgtm` green. Manual: cmd-d, chip → devbox, cmd-enter; on the devbox `tmux list-windows -t claude` shows `#N` in `~/leap/<repo>`. Toggle Codex, repeat into `codex` session. Rename the repo folder → red "no checkout" line, card stays.
