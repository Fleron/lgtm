mod app;
mod cached_prs;
mod chat;
mod comments;
mod composer;
mod diff;
mod items;
mod lsp;
mod lsp_client;
mod minimap;
mod palette;
mod selection;
mod subscriptions;
mod theme;
mod titlebar;
mod sidebar;
mod tree;

use gpui::{
    actions, prelude::*, px, size, App,
    Application, Bounds, Context, KeyBinding,
    Task, TitlebarOptions,
    Window, WindowBounds, WindowOptions,
};
use gpui_component::{
    input::{CompletionProvider, InputState},
    Root, Rope, RopeExt as _, TitleBar,
};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use cached_prs::git_env;

pub(crate) use diff::{
    line_content, row_height, text_size, Cell, CardEdge, LineKind, Row,
    ViewMode, MAX_SOURCE_HIGHLIGHT_BYTES, MAX_SYNTAX_LINE_BYTES,
    SPLIT_DIVIDER,
};

pub(crate) use app::{app_title, centered_message, ReviewApp};
pub(crate) use items::{ItemData, Source};
pub(crate) use palette::SIDEBAR_MAX_LIST_HEIGHT;

pub(crate) use chat::{chat_scratch_root, local_review_prompt, ChatState};
pub(crate) use lsp::{HoverState, LspHandle, NavLocation, SourceViewState, SymbolTarget};

pub(crate) const MONO: &str = "Menlo";

actions!(
    lgtm,
    [
        NextFile,
        PrevFile,
        NextHunk,
        PrevHunk,
        GoToTop,
        GoToBottom,
        ToggleView,
        Quit,
        ToggleSidebar,
        OpenInput,
        CloseItem,
        NextItem,
        PrevItem,
        Refresh,
        OpenPalette,
        PaletteUp,
        PaletteDown,
        PaletteBack,
        ClearSelection,
        CopySelection,
        FocusTreeFilter,
        ToggleMinimap,
        ToggleComments,
        ToggleChat,
        SubmitReview,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        GoToDefinition,
        NavBack,
        NavForward
    ]
);

fn main() {
    // Before any thread or child exists: set_var is process-global and not
    // thread-safe, and every subprocess needs to inherit this.
    for (key, value) in git_env() {
        std::env::set_var(key, value);
    }
    // Keep an SSH remote from stalling on a passphrase prompt for the same
    // reason — but never clobber a wrapper the user configured themselves
    // (custom identity, proxy command), which would break their setup.
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        std::env::set_var("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [mode, root] = args.as_slice() {
        if mode == "--bifrost-lsp-server" {
            if let Err(err) = brokk_bifrost::lsp::run_lsp_stdio_server(PathBuf::from(root)) {
                eprintln!("lgtm: embedded Bifrost LSP failed: {err}");
                std::process::exit(1);
            }
            return;
        }
    }
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    if args.is_empty() {
        // No args: review the repo we're standing in, or open empty.
        if let Ok(src) = git::resolve_local(Path::new(".")) {
            sources.push(Source::Local(src));
        }
    } else {
        for arg in &args {
            let parsed = if Path::new(arg).is_dir() {
                git::resolve_local(Path::new(arg)).map(Source::Local)
            } else {
                gh::resolve_pr_arg(arg).map(Source::Pr)
            };
            match parsed {
                Ok(source) => sources.push(source),
                Err(err) => {
                    eprintln!("error: {arg}: {err:#}");
                    errors.push(format!("{arg}: {err:#}"));
                }
            }
        }
    }

    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::apply_ui_theme(cx);
            cx.bind_keys([
                KeyBinding::new("]", NextFile, Some("ReviewApp")),
                KeyBinding::new("[", PrevFile, Some("ReviewApp")),
                KeyBinding::new("n", NextHunk, Some("ReviewApp")),
                KeyBinding::new("p", PrevHunk, Some("ReviewApp")),
                KeyBinding::new("home", GoToTop, Some("ReviewApp")),
                KeyBinding::new("end", GoToBottom, Some("ReviewApp")),
                KeyBinding::new("v", ToggleView, Some("ReviewApp")),
                KeyBinding::new("m", ToggleMinimap, Some("ReviewApp")),
                KeyBinding::new("c", ToggleComments, Some("ReviewApp")),
                KeyBinding::new("r", Refresh, Some("ReviewApp")),
                // Finish the review: approve / request changes / comment.
                KeyBinding::new("cmd-enter", SubmitReview, Some("ReviewApp")),
                // Only while the diff pane has focus; typing `/` in any input
                // stays a plain character.
                KeyBinding::new("/", FocusTreeFilter, Some("ReviewApp")),
                // Selection: escape/cmd-c only fire while the diff pane has
                // focus; with the palette open its input has focus, so the
                // palette's own escape routing wins by construction.
                KeyBinding::new("escape", ClearSelection, Some("ReviewApp")),
                KeyBinding::new("cmd-c", CopySelection, Some("ReviewApp")),
                KeyBinding::new("f12", GoToDefinition, Some("ReviewApp")),
                KeyBinding::new("ctrl-tab", NextItem, Some("ReviewApp")),
                KeyBinding::new("ctrl-shift-tab", PrevItem, Some("ReviewApp")),
                KeyBinding::new("cmd-left", NavBack, Some("ReviewApp")),
                KeyBinding::new("cmd-right", NavForward, Some("ReviewApp")),
                KeyBinding::new("alt-left", NavBack, Some("ReviewApp")),
                KeyBinding::new("alt-right", NavForward, Some("ReviewApp")),
                // Zoom. Bind both "cmd-=" and "cmd-+" so it fires with or
                // without shift, the way browsers and editors behave.
                KeyBinding::new("cmd-=", ZoomIn, None),
                KeyBinding::new("cmd-+", ZoomIn, None),
                KeyBinding::new("cmd--", ZoomOut, None),
                KeyBinding::new("cmd-0", ZoomReset, None),
                // Global (None context): must work while the open input is focused.
                KeyBinding::new("cmd-b", ToggleSidebar, None),
                KeyBinding::new("cmd-j", ToggleChat, None),
                KeyBinding::new("cmd-t", OpenInput, None),
                KeyBinding::new("cmd-w", CloseItem, None),
                KeyBinding::new("cmd-k", OpenPalette, None),
                KeyBinding::new("cmd-q", Quit, None),
                // Palette navigation. The `Palette > Input` variants are bound
                // after gpui_component::init, so at the input's dispatch depth
                // they take precedence over the Input's own up/down (which a
                // single-line input consumes without propagating).
                KeyBinding::new("up", PaletteUp, Some("Palette")),
                KeyBinding::new("down", PaletteDown, Some("Palette")),
                KeyBinding::new("escape", PaletteBack, Some("Palette")),
                KeyBinding::new("up", PaletteUp, Some("Palette > Input")),
                KeyBinding::new("down", PaletteDown, Some("Palette > Input")),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            // One window is the whole app: closing it quits the process.
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1280.), px(860.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("lgtm".into()),
                        ..TitleBar::title_bar_options()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| ReviewApp::new(sources, errors, window, cx));
                    window.focus(&view.read(cx).focus_handle);
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .unwrap();
            cx.activate(true);
        });
}

// --- @-mention autocomplete ------------------------------------------------

/// Most completion items to offer at once.
const MENTION_LIMIT: usize = 50;

/// `@`-mention autocomplete for the comment composer, backed by the item's
/// shared, live-updating pool of mentionable users (seeded with PR
/// participants, then filled from the repo's mentionable set in the
/// background). Reads the pool fresh on every keystroke.
pub(crate) struct MentionProvider {
    pub(crate) users: Rc<RefCell<Vec<gh::Mention>>>,
}

/// If the cursor sits inside an `@mention` token, return the byte offset of the
/// `@` and the (possibly empty) login text typed after it. The `@` must begin a
/// word — preceded by whitespace or the start of the text — matching GitHub's
/// own mention rules, so `foo@bar` never triggers.
fn mention_prefix(text: &Rope, offset: usize) -> Option<(usize, String)> {
    let s = text.to_string();
    let offset = offset.min(s.len());
    let before = &s[..offset];
    // GitHub logins are alphanumeric plus hyphen; walk back over that run.
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '-')
        .last()
        .map(|(i, _)| i)
        .unwrap_or(offset);
    if start == 0 || before.as_bytes()[start - 1] != b'@' {
        return None;
    }
    let at = start - 1;
    if at > 0 && !before[..at].chars().next_back().unwrap().is_whitespace() {
        return None;
    }
    Some((at, before[start..offset].to_string()))
}

/// Rank of `user` against `query` (matched case-insensitively), lower = better;
/// None = no match. Login prefix beats name prefix beats substring matches.
fn mention_rank(user: &gh::Mention, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let query = &query.to_ascii_lowercase();
    let login = user.login.to_ascii_lowercase();
    let name = user.name.as_ref().map(|n| n.to_ascii_lowercase());
    if login.starts_with(query) {
        Some(0)
    } else if name
        .as_deref()
        .is_some_and(|n| n.split_whitespace().any(|w| w.starts_with(query)))
    {
        Some(1)
    } else if login.contains(query) {
        Some(2)
    } else if name.as_deref().is_some_and(|n| n.contains(query)) {
        Some(3)
    } else {
        None
    }
}

impl CompletionProvider for MentionProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: lsp_types::CompletionContext,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        let empty = Task::ready(Ok(lsp_types::CompletionResponse::Array(vec![])));
        let Some((at, prefix)) = mention_prefix(text, offset) else {
            return empty;
        };
        // The edit replaces `@prefix` (the token so far) with `@login `.
        let range = lsp_types::Range {
            start: text.offset_to_position(at),
            end: text.offset_to_position(offset),
        };
        let users = self.users.borrow();
        let mut ranked: Vec<(u8, &gh::Mention)> = users
            .iter()
            .filter_map(|u| mention_rank(u, &prefix).map(|r| (r, u)))
            .collect();
        // Stable sort keeps GitHub's alphabetical order within each rank.
        ranked.sort_by_key(|(rank, _)| *rank);
        let items = ranked
            .into_iter()
            .take(MENTION_LIMIT)
            .map(|(_, u)| lsp_types::CompletionItem {
                label: u.login.clone(),
                filter_text: Some(u.login.clone()),
                detail: u.name.clone(),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range,
                    new_text: format!("@{} ", u.login),
                })),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        Task::ready(Ok(lsp_types::CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(
        &self,
        _offset: usize,
        _new_text: &str,
        _cx: &mut Context<InputState>,
    ) -> bool {
        // Cheap to always run; `completions` returns nothing outside a mention.
        true
    }
}

/// Seed `cell` with everyone already visible on the PR — the author and every
/// comment author — so autocomplete has relevant names before the full
/// mentionable-user fetch returns. Additive: never drops fetched entries.
pub(crate) fn seed_mentions(cell: &Rc<RefCell<Vec<gh::Mention>>>, data: &ItemData) {
    let mut pool = cell.borrow_mut();
    let mut add = |login: &str| {
        if !login.is_empty() && !pool.iter().any(|m| m.login.eq_ignore_ascii_case(login)) {
            pool.push(gh::Mention {
                login: login.to_string(),
                name: None,
            });
        }
    };
    if let Some(meta) = &data.pr_meta {
        add(&meta.author.login);
    }
    if let Some(index) = &data.comments {
        for anchors in index.threads.values() {
            for threads in anchors.values() {
                for thread in threads {
                    add(&thread.root.user.login);
                    for reply in &thread.replies {
                        add(&reply.user.login);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use diff_core::{DiffRow, FileDiff, FileStatus, Hunk, PrDiff};
    use gpui::HighlightStyle;
    use std::ops::Range;
    use syntax::Token;

    pub(crate) fn ctx(old_no: u32, new_no: u32, text: &str) -> DiffRow {
        DiffRow::Context {
            old_no,
            new_no,
            text: text.to_string(),
        }
    }

    pub(crate) fn add(new_no: u32, text: &str, intra: Vec<Range<usize>>) -> DiffRow {
        DiffRow::Added {
            new_no,
            text: text.to_string(),
            intra,
        }
    }

    pub(crate) fn rem(old_no: u32, text: &str, intra: Vec<Range<usize>>) -> DiffRow {
        DiffRow::Removed {
            old_no,
            text: text.to_string(),
            intra,
        }
    }

    pub(crate) fn hunk(old_start: u32, new_start: u32, rows: Vec<DiffRow>) -> Hunk {
        Hunk {
            old_start,
            old_count: 0,
            new_start,
            new_count: 0,
            section: String::new(),
            rows,
        }
    }

    pub(crate) fn sample_diff() -> PrDiff {
        PrDiff {
            files: vec![
                FileDiff {
                    old_path: Some("a.rs".into()),
                    new_path: Some("a.rs".into()),
                    status: FileStatus::Modified,
                    hunks: vec![
                        // Equal-count modified run, flanked by context.
                        hunk(
                            1,
                            1,
                            vec![
                                ctx(1, 1, "ctx"),
                                rem(2, "old1", vec![0..3]),
                                rem(3, "old2", Vec::new()),
                                add(2, "new1", vec![0..3]),
                                add(3, "new2", Vec::new()),
                                ctx(4, 4, "tail"),
                            ],
                        ),
                        // Unequal run (2 removed, 1 added) + a lone added run.
                        hunk(
                            10,
                            10,
                            vec![
                                rem(10, "r1", Vec::new()),
                                rem(11, "r2", Vec::new()),
                                add(10, "a1", Vec::new()),
                                ctx(12, 11, "c"),
                                add(12, "lone", Vec::new()),
                            ],
                        ),
                    ],
                    additions: 4,
                    deletions: 4,
                },
                FileDiff {
                    old_path: Some("b.png".into()),
                    new_path: Some("b.png".into()),
                    status: FileStatus::Binary,
                    hunks: Vec::new(),
                    additions: 0,
                    deletions: 0,
                },
            ],
        }
    }

    pub(crate) fn cell(cell: &Option<Cell>) -> (u32, LineKind, &str, &[Range<usize>]) {
        let cell = cell.as_ref().expect("expected a cell");
        (cell.no, cell.kind, cell.text.as_ref(), &cell.intra)
    }

    pub(crate) fn style(token: Token) -> HighlightStyle {
        theme::token_style(token)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rc(
        id: u64,
        path: &str,
        side: Option<&str>,
        line: Option<u64>,
        body: &str,
        author: &str,
        created_at: &str,
        reply_to: Option<u64>,
    ) -> gh::ReviewComment {
        gh::ReviewComment {
            id,
            path: path.to_string(),
            line,
            side: side.map(str::to_string),
            start_line: None,
            body: body.to_string(),
            user: gh::Author {
                login: author.to_string(),
            },
            created_at: created_at.to_string(),
            in_reply_to_id: reply_to,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use chat::{
        chat_prompt, local_chat_header, materialize_files, pr_chat_header, scratch_path,
        truncate_str, MAX_CHAT_PATCH_BYTES, MAX_EXPLORE_FILE_BYTES,
    };
    use comments::{group_comments, CommentIndex, CommentSide, LocalReview, COMMENT_WRAP_CHARS};
    use lsp::identifier_start_at;
    use palette::{filter_prs, filtered_sources};
    use diff::{
        build_rows, gap_span, hunk_syntax, is_comment_row, merge_highlights,
        nth_noncomment_row, row_height_for, FileUpgrade,
    };
    use diff::{DEFAULT_TEXT_SIZE, MAX_TEXT_SIZE, MIN_TEXT_SIZE};
    use diff_core::{diff_texts, FileDiff, FileStatus, PrDiff};
    use gpui::{HighlightStyle, Keystroke, SharedString};
    use minimap::{minimap_rows, MinimapKind, MinimapRow};
    use selection::{row_side_text, SelSide};
    use std::collections::HashMap;
    use test_util::*;
    use comments::comment_anchor;

    fn mention(login: &str, name: Option<&str>) -> gh::Mention {
        gh::Mention {
            login: login.to_string(),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn mention_prefix_detects_at_tokens_at_word_boundaries() {
        let at = |s: &str, off: usize| mention_prefix(&Rope::from(s), off);
        // Bare `@` with the cursor right after it: empty prefix.
        assert_eq!(at("hi @", 4), Some((3, String::new())));
        // Mid-token cursor returns only what's typed so far.
        assert_eq!(at("hi @oct", 7), Some((3, "oct".to_string())));
        assert_eq!(at("hi @oct", 5), Some((3, "o".to_string())));
        // Start of text counts as a boundary.
        assert_eq!(at("@oct", 4), Some((0, "oct".to_string())));
        // Hyphens are valid login characters.
        assert_eq!(at("@foo-bar", 8), Some((0, "foo-bar".to_string())));
        // Not a boundary (looks like an email) — no completion.
        assert_eq!(at("foo@bar", 7), None);
        // No `@` at all.
        assert_eq!(at("hello", 5), None);
        // Cursor before the `@`.
        assert_eq!(at("hi @oct", 3), None);
    }

    #[test]
    fn mention_rank_orders_login_prefix_first() {
        let octocat = mention("octocat", Some("The Octocat"));
        // Login prefix is the strongest match.
        assert_eq!(mention_rank(&octocat, "oct"), Some(0));
        // Empty query matches everything at the top rank.
        assert_eq!(mention_rank(&octocat, ""), Some(0));
        // Name-word prefix beats a login substring.
        assert_eq!(mention_rank(&mention("xyz", Some("Bob Jones")), "bob"), Some(1));
        assert_eq!(mention_rank(&mention("abobc", None), "bob"), Some(2));
        // Case-insensitive, and no match returns None.
        assert_eq!(mention_rank(&octocat, "OCT"), Some(0));
        assert_eq!(mention_rank(&octocat, "zzz"), None);
    }

    #[test]
    fn identifier_hover_hit_snaps_to_the_token_start() {
        let text = "    if cfg.font.mono_family.is_empty() {";
        assert_eq!(identifier_start_at(text, 7), Some(7));
        assert_eq!(identifier_start_at(text, 9), Some(7));
        assert_eq!(identifier_start_at(text, 10), Some(7));
        assert_eq!(identifier_start_at(text, 11), Some(11));
        assert_eq!(identifier_start_at(text, 14), Some(11));
        assert_eq!(identifier_start_at(text, 15), Some(11));
        assert_eq!(identifier_start_at(text, 6), None);
    }

    #[test]
    fn split_context_fills_both_cells() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        // rows[0] = FileHeader, rows[1] = HunkHeader, rows[2] = first context.
        match &rows[2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (1, LineKind::Context, "ctx", &[][..]));
                assert_eq!(cell(right), (1, LineKind::Context, "ctx", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_pairs_equal_runs_positionally() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        match &rows[3] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (2, LineKind::Removed, "old1", &[0..3][..]));
                assert_eq!(cell(right), (2, LineKind::Added, "new1", &[0..3][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[4] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (3, LineKind::Removed, "old2", &[][..]));
                assert_eq!(cell(right), (3, LineKind::Added, "new2", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        // Equal run + 2 context rows: 4 split lines for a 6-row hunk.
        match &rows[5] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left).2, "tail");
                assert_eq!(cell(right).2, "tail");
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_unequal_and_lone_runs_are_one_sided() {
        let (rows, _, hunk_rows) =
            build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let h2 = hunk_rows[1];
        // 2 removed / 1 added: first row paired, second left-only.
        match &rows[h2 + 1] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (10, LineKind::Removed, "r1", &[][..]));
                assert_eq!(cell(right), (10, LineKind::Added, "a1", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[h2 + 2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (11, LineKind::Removed, "r2", &[][..]));
                assert!(right.is_none());
            }
            _ => panic!("expected split line"),
        }
        // Lone added run after context: right-only.
        match &rows[h2 + 4] {
            Row::SplitLine { left, right } => {
                assert!(left.is_none());
                assert_eq!(cell(right), (12, LineKind::Added, "lone", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    /// A 20-line file with line 10 changed, re-diffed with context 3: one
    /// hunk covering lines 7..=13, hidden gaps of 6 lines above and 7 below.
    fn upgraded_diff() -> (PrDiff, HashMap<usize, FileUpgrade>) {
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 10\n", "line ten\n");
        let hunks = diff_texts(&old, &new, 3);
        let new_lines: Vec<SharedString> = new.lines().map(|l| l.to_string().into()).collect();
        let n = new_lines.len();
        let file = FileDiff {
            old_path: Some("a.txt".into()),
            new_path: Some("a.txt".into()),
            status: FileStatus::Modified,
            hunks,
            additions: 1,
            deletions: 1,
        };
        let upgrade = FileUpgrade {
            new_lines,
            old_spans: vec![Vec::new(); 20],
            new_spans: vec![Vec::new(); n],
            expanded: HashSet::new(),
        };
        (PrDiff { files: vec![file] }, HashMap::from([(0, upgrade)]))
    }

    #[test]
    fn gap_span_math() {
        let (diff, upgrades) = upgraded_diff();
        let hunks = &diff.files[0].hunks;
        let total = upgrades[&0].new_lines.len() as u32;
        assert_eq!(gap_span(hunks, 0, total), (1, 1, 6)); // lines 1..=6 hidden
        assert_eq!(gap_span(hunks, 1, total), (14, 14, 7)); // lines 14..=20

        // Zero hunks (e.g. a pure CRLF flip): the whole file is one gap.
        assert_eq!(gap_span(&[], 0, total), (1, 1, 20));
        // Added file: single hunk covers everything, both gaps empty.
        let added = diff_texts("", "a\nb\n", 3);
        assert_eq!(gap_span(&added, 0, 2).2, 0);
        assert_eq!(gap_span(&added, 1, 2).2, 0);
        // Deleted file: new side empty, no gaps and no underflow.
        let deleted = diff_texts("a\nb\n", "", 3);
        assert_eq!(gap_span(&deleted, 0, 0).2, 0);
        assert_eq!(gap_span(&deleted, 1, 0).2, 0);
    }

    fn row_name(row: &Row) -> &'static str {
        match row {
            Row::Spacer => "Spacer",
            Row::FileHeader { .. } => "FileHeader",
            Row::HunkHeader { .. } => "HunkHeader",
            Row::Binary => "Binary",
            Row::Gap { .. } => "Gap",
            Row::Line { .. } => "Line",
            Row::SplitLine { .. } => "SplitLine",
            Row::CommentHeader { .. } => "CommentHeader",
            Row::CommentBody { .. } => "CommentBody",
            Row::CommentActions { .. } => "CommentActions",
        }
    }

    #[test]
    fn upgraded_file_gets_gap_rows_and_marked_headers() {
        let (diff, upgrades) = upgraded_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, _, hunk_rows) = build_rows(&diff, mode, &upgrades, None, true, COMMENT_WRAP_CHARS);
            // FileHeader, Gap(6), HunkHeader, 7 hunk rows, Gap(7).
            match &rows[1] {
                Row::Gap {
                    file_ix,
                    gap_ix,
                    hidden,
                } => {
                    assert_eq!((*file_ix, *gap_ix, *hidden), (0, 0, 6));
                }
                other => panic!("expected leading gap, got {}", row_name(other)),
            }
            assert_eq!(hunk_rows, vec![2]);
            assert!(matches!(rows[2], Row::HunkHeader { upgraded: true, .. }));
            match rows.last().unwrap() {
                Row::Gap { gap_ix, hidden, .. } => assert_eq!((*gap_ix, *hidden), (1, 7)),
                other => panic!("expected trailing gap, got {}", row_name(other)),
            }
            // Gap rows are selectable-through, like headers.
            assert!(row_side_text(&rows[1], SelSide::Unified).is_none());
            assert!(row_side_text(&rows[1], SelSide::Left).is_none());
        }
        // Un-upgraded build of the same diff has no gap rows.
        let (rows, _, _) = build_rows(&diff, ViewMode::Unified, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        assert!(!rows.iter().any(|row| matches!(row, Row::Gap { .. })));
        assert!(matches!(
            rows[1],
            Row::HunkHeader {
                upgraded: false,
                ..
            }
        ));
    }

    #[test]
    fn expanded_gap_synthesizes_context_rows_with_correct_numbers() {
        let (diff, mut upgrades) = upgraded_diff();
        upgrades.get_mut(&0).unwrap().expanded.insert(0);

        let (rows, _, hunk_rows) = build_rows(&diff, ViewMode::Unified, &upgrades, None, true, COMMENT_WRAP_CHARS);
        // Leading gap expanded into 6 context rows before the hunk header.
        assert_eq!(hunk_rows, vec![7]); // FileHeader + 6 context rows
        for (j, row) in rows[1..7].iter().enumerate() {
            match row {
                Row::Line {
                    old_no,
                    new_no,
                    kind,
                    text,
                    ..
                } => {
                    assert_eq!(*kind, LineKind::Context);
                    assert_eq!(*old_no, Some(j as u32 + 1));
                    assert_eq!(*new_no, Some(j as u32 + 1));
                    assert_eq!(text.as_ref(), format!("line {}", j + 1));
                }
                other => panic!("expected context line, got {}", row_name(other)),
            }
        }
        // Trailing gap still collapsed.
        assert!(matches!(rows.last(), Some(Row::Gap { gap_ix: 1, .. })));

        // Split mode: same expansion as two-cell context rows.
        let (rows, _, _) = build_rows(&diff, ViewMode::Split, &upgrades, None, true, COMMENT_WRAP_CHARS);
        match &rows[1] {
            Row::SplitLine { left, right } => {
                let (l, r) = (left.as_ref().unwrap(), right.as_ref().unwrap());
                assert_eq!((l.no, r.no), (1, 1));
                assert_eq!(l.kind, LineKind::Context);
                assert_eq!(l.text.as_ref(), "line 1");
                assert_eq!(r.text.as_ref(), "line 1");
            }
            other => panic!("expected split context, got {}", row_name(other)),
        }

        // Expanding the trailing gap too: numbering continues past the hunk.
        upgrades.get_mut(&0).unwrap().expanded.insert(1);
        let (rows, _, _) = build_rows(&diff, ViewMode::Unified, &upgrades, None, true, COMMENT_WRAP_CHARS);
        assert!(!rows.iter().any(|row| matches!(row, Row::Gap { .. })));
        match rows.last().unwrap() {
            Row::Line {
                old_no,
                new_no,
                text,
                ..
            } => {
                assert_eq!((*old_no, *new_no), (Some(20), Some(20)));
                assert_eq!(text.as_ref(), "line 20");
            }
            other => panic!("expected context line, got {}", row_name(other)),
        }
    }

    use syntax::Token;

    fn style_bg(token: Option<Token>) -> HighlightStyle {
        let mut style = token.map(style).unwrap_or_default();
        style.background_color = Some(theme::added_word_bg().into());
        style
    }

    #[test]
    fn merge_syntax_only() {
        let syntax = [(0..2, Token::Keyword), (3..7, Token::Function)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, None),
            vec![
                (0..2, style(Token::Keyword)),
                (3..7, style(Token::Function))
            ]
        );
    }

    #[test]
    fn merge_intra_only() {
        let bg = Some(theme::added_word_bg());
        assert_eq!(
            merge_highlights(&[], &[2..5], bg, None),
            vec![(2..5, style_bg(None))]
        );
    }

    #[test]
    fn merge_partial_overlap() {
        let bg = Some(theme::added_word_bg());
        let syntax = [(0..6, Token::String)];
        assert_eq!(
            merge_highlights(&syntax, &[4..8], bg, None),
            vec![
                (0..4, style(Token::String)),
                (4..6, style_bg(Some(Token::String))),
                (6..8, style_bg(None)),
            ]
        );
    }

    #[test]
    fn merge_intra_spanning_multiple_tokens() {
        let bg = Some(theme::added_word_bg());
        let syntax = [(0..3, Token::Keyword), (5..8, Token::Number)];
        assert_eq!(
            merge_highlights(&syntax, &[1..7], bg, None),
            vec![
                (0..1, style(Token::Keyword)),
                (1..3, style_bg(Some(Token::Keyword))),
                (3..5, style_bg(None)),
                (5..7, style_bg(Some(Token::Number))),
                (7..8, style(Token::Number)),
            ]
        );
    }

    #[test]
    fn merge_adjacent_ranges() {
        // Same style across a shared boundary coalesces; different styles
        // stay split exactly at the boundary.
        let syntax = [(0..2, Token::Keyword), (2..4, Token::Keyword)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, None),
            vec![(0..4, style(Token::Keyword))]
        );
        let syntax = [(0..2, Token::Keyword), (2..4, Token::Type)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, None),
            vec![(0..2, style(Token::Keyword)), (2..4, style(Token::Type))]
        );
        let bg = Some(theme::added_word_bg());
        assert_eq!(
            merge_highlights(&[], &[0..2, 2..4], bg, None),
            vec![(0..4, style_bg(None))]
        );
    }

    #[test]
    fn hunk_syntax_takes_spans_from_the_right_side() {
        let lang = syntax::language_for_path("x.rs");
        let rows = vec![
            ctx(1, 1, "fn f() {"),
            rem(2, "// gone", Vec::new()),
            add(2, "    let b = 2;", Vec::new()),
            ctx(3, 3, "}"),
        ];
        let spans = hunk_syntax(lang, &rows);
        assert_eq!(spans.len(), 4);
        // Context: from the new side.
        assert!(spans[0].contains(&(0..2, Token::Keyword)));
        // Removed: highlighted as part of old_source — a comment.
        assert_eq!(spans[1], vec![(0..7, Token::Comment)]);
        // Added: highlighted as part of new_source — `let` keyword.
        assert!(spans[2].contains(&(4..7, Token::Keyword)));
    }

    #[test]
    fn hunk_syntax_guardrails() {
        let rows = vec![ctx(1, 1, "fn f() {}")];
        // No language → no spans.
        assert_eq!(hunk_syntax(None, &rows), vec![Vec::new()]);
        // Over-long line stays plain even when the hunk is highlighted.
        let lang = syntax::language_for_path("x.rs");
        let long = format!("// {}", "x".repeat(5000));
        let rows = vec![ctx(1, 1, "fn f() {}"), ctx(2, 2, &long)];
        let spans = hunk_syntax(lang, &rows);
        assert!(!spans[0].is_empty());
        assert!(spans[1].is_empty());
    }

    fn pr(number: u64, title: &str, author: &str, branch: &str) -> gh::PrSummary {
        gh::PrSummary {
            number,
            title: title.to_string(),
            author: gh::Author {
                login: author.to_string(),
            },
            state: "OPEN".to_string(),
            is_draft: false,
            head_ref_name: branch.to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
            review_decision: String::new(),
            status_check_rollup: Vec::new(),
        }
    }

    #[test]
    fn pr_filter_empty_query_keeps_original_order() {
        let all = vec![
            pr(3, "fix crash", "alice", "fix-crash"),
            pr(1, "add feature", "bob", "feat"),
            pr(2, "docs", "carol", "docs"),
        ];
        assert_eq!(filter_prs(&all, ""), vec![0, 1, 2]);
        assert_eq!(filter_prs(&all, "   "), vec![0, 1, 2]);
    }

    #[test]
    fn pr_filter_matches_number_title_author_and_branch() {
        let all = vec![
            pr(3, "fix crash", "alice", "fix-crash"),
            pr(1, "add feature", "bob", "feat"),
        ];
        assert_eq!(filter_prs(&all, "#3"), vec![0]);
        assert_eq!(filter_prs(&all, "bob"), vec![1]);
        assert_eq!(filter_prs(&all, "crash"), vec![0]);
        assert!(filter_prs(&all, "zzzqqq").is_empty());
    }

    #[test]
    fn source_options_filter_by_substring() {
        assert_eq!(filtered_sources(""), vec![0, 1, 2]);
        assert_eq!(filtered_sources("pull"), vec![0]);
        assert_eq!(filtered_sources("subscribe"), vec![1]);
        assert_eq!(filtered_sources("FOLDER"), vec![2]);
        assert_eq!(filtered_sources("open"), vec![0, 2]);
        assert!(filtered_sources("nope").is_empty());
    }

    #[test]
    fn header_indices_are_correct_in_both_modes() {
        let diff = sample_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, file_rows, hunk_rows) = build_rows(&diff, mode, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
            assert_eq!(file_rows.len(), 2);
            assert_eq!(hunk_rows.len(), 2);
            for &ix in &file_rows {
                assert!(matches!(rows[ix], Row::FileHeader { .. }));
            }
            for &ix in &hunk_rows {
                assert!(matches!(rows[ix], Row::HunkHeader { .. }));
            }
            // Binary file: header immediately followed by the binary row.
            assert!(matches!(rows[file_rows[1] + 1], Row::Binary));
        }
        // Unified emits one row per diff row; split collapses the equal run.
        let (unified, _, _) = build_rows(&diff, ViewMode::Unified, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let (split, _, _) = build_rows(&diff, ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let unified_lines = unified
            .iter()
            .filter(|r| matches!(r, Row::Line { .. }))
            .count();
        let split_lines = split
            .iter()
            .filter(|r| matches!(r, Row::SplitLine { .. }))
            .count();
        assert_eq!(unified_lines, 11);
        assert_eq!(split_lines, 8); // 4 (hunk 1) + 4 (hunk 2)
    }

    // --- Minimap -----------------------------------------------------------

    fn mrow(kind: MinimapKind, len_frac: f32) -> MinimapRow {
        MinimapRow { kind, len_frac }
    }

    #[test]
    fn minimap_rows_unified_kinds_and_fracs() {
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
                COMMENT_WRAP_CHARS,
        );
        let mm = minimap_rows(&rows);
        assert_eq!(mm.len(), rows.len());
        // FileHeader and HunkHeader both map to Header ticks.
        assert_eq!(mm[0], mrow(MinimapKind::Header, 1.));
        assert_eq!(mm[1], mrow(MinimapKind::Header, 1.));
        // Line rows: kind from the row, frac = chars / MAX_MINIMAP_CHARS.
        assert_eq!(mm[2], mrow(MinimapKind::Context, 3. / 160.)); // "ctx"
        assert_eq!(mm[3], mrow(MinimapKind::Removed, 4. / 160.)); // "old1"
        assert_eq!(mm[5], mrow(MinimapKind::Added, 4. / 160.)); // "new1"
                                                                // Spacer and Binary rows are blank.
        assert_eq!(mm[14], mrow(MinimapKind::Blank, 0.));
        assert_eq!(mm[16], mrow(MinimapKind::Blank, 0.));
    }

    #[test]
    fn minimap_rows_split_pairs_and_gaps() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let mm = minimap_rows(&rows);
        assert_eq!(mm.len(), rows.len());
        // Context pair: both halves, no change flags.
        assert_eq!(
            mm[2].kind,
            MinimapKind::SplitPair {
                left_frac: 3. / 160.,
                right_frac: 3. / 160.,
                left: false,
                right: false,
            }
        );
        // Paired removed/added ("old1" / "new1").
        assert_eq!(
            mm[3].kind,
            MinimapKind::SplitPair {
                left_frac: 4. / 160.,
                right_frac: 4. / 160.,
                left: true,
                right: true,
            }
        );
        assert_eq!(mm[3].len_frac, 4. / 160.);
        // One-sided rows: the absent half has a zero fraction and no flag.
        let h2 = 6; // second HunkHeader (see split tests above)
        assert_eq!(
            mm[h2 + 2].kind,
            MinimapKind::SplitPair {
                left_frac: 2. / 160., // "r2"
                right_frac: 0.,
                left: true,
                right: false,
            }
        );
        assert_eq!(
            mm[h2 + 4].kind,
            MinimapKind::SplitPair {
                left_frac: 0.,
                right_frac: 4. / 160., // "lone"
                left: false,
                right: true,
            }
        );

        // Gap rows map to Gap in both modes.
        let (diff, upgrades) = upgraded_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, _, _) = build_rows(&diff, mode, &upgrades, None, true, COMMENT_WRAP_CHARS);
            let mm = minimap_rows(&rows);
            assert_eq!(mm[1], mrow(MinimapKind::Gap, 1.));
        }
    }

    // --- Review comments ----------------------------------------------------

    /// Comments for sample_diff's a.rs: a RIGHT thread (with one reply) on
    /// added line 2 ("new1"), a LEFT thread on removed line 2 ("old1"), and
    /// one outdated comment.
    fn sample_comments() -> CommentIndex {
        group_comments(vec![
            rc(
                1,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "on new1",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            rc(
                2,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "reply",
                "bob",
                "2026-01-02T00:00:00Z",
                Some(1),
            ),
            rc(
                3,
                "a.rs",
                Some("LEFT"),
                Some(2),
                "on old1",
                "carol",
                "2026-01-03T00:00:00Z",
                None,
            ),
            rc(
                4,
                "a.rs",
                None,
                None,
                "outdated",
                "dave",
                "2026-01-04T00:00:00Z",
                None,
            ),
        ])
    }

    #[test]
    fn unified_rows_insert_threads_beneath_their_anchor() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // rows: FileHeader, HunkHeader, ctx, rem old1 (old_no 2) + LEFT
        // thread, rem old2, add new1 (new_no 2) + RIGHT thread, …
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        assert_eq!(
            &names[..12],
            &[
                "FileHeader",
                "HunkHeader",
                "Line",          // ctx 1/1
                "Line",          // rem old1 (old 2)
                "CommentHeader", // carol on old1
                "CommentBody",
                "CommentActions",
                "Line",          // rem old2
                "Line",          // add new1 (new 2)
                "CommentHeader", // alice
                "CommentBody",
                "CommentHeader", // bob's reply
            ]
        );
        assert_eq!(names[12], "CommentBody");
        assert_eq!(names[13], "CommentActions");
        // The LEFT thread is carol's; the reply flag follows position.
        match &rows[4] {
            Row::CommentHeader {
                author, is_reply, ..
            } => {
                assert_eq!(author.as_ref(), "carol");
                assert!(!is_reply);
            }
            other => panic!("expected comment header, got {}", row_name(other)),
        }
        match &rows[11] {
            Row::CommentHeader {
                author, is_reply, ..
            } => {
                assert_eq!(author.as_ref(), "bob");
                assert!(is_reply);
            }
            other => panic!("expected reply header, got {}", row_name(other)),
        }
        // Actions row targets the thread root and carries the anchor.
        match &rows[13] {
            Row::CommentActions {
                root_id,
                path,
                side,
                line,
                ..
            } => {
                assert_eq!(*root_id, 1);
                assert_eq!(path.as_ref(), "a.rs");
                assert_eq!((*side, *line), (CommentSide::Right, 2));
            }
            other => panic!("expected actions row, got {}", row_name(other)),
        }
        // File header carries the counts (3 anchored, 1 outdated).
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (3, 1));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
        // Comment rows are selectable-through and blank in the minimap.
        assert!(row_side_text(&rows[4], SelSide::Unified).is_none());
        assert_eq!(minimap_rows(&rows)[4], mrow(MinimapKind::Blank, 0.));
    }

    #[test]
    fn split_rows_anchor_threads_by_cell() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // rows[3] pairs old1/new1 (both line 2): LEFT thread then RIGHT
        // thread directly beneath it.
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        assert_eq!(
            &names[2..11],
            &[
                "SplitLine",     // ctx
                "SplitLine",     // old1 | new1
                "CommentHeader", // carol (LEFT)
                "CommentBody",
                "CommentActions",
                "CommentHeader", // alice (RIGHT)
                "CommentBody",
                "CommentHeader", // bob reply
                "CommentBody",
            ]
        );
        assert_eq!(names[11], "CommentActions");
        assert_eq!(names[12], "SplitLine"); // old2 | new2
    }

    /// The half a comment row renders in, or None for a full-width card.
    fn row_half(row: &Row) -> Option<Option<CommentSide>> {
        match row {
            Row::CommentHeader { half, .. }
            | Row::CommentBody { half, .. }
            | Row::CommentActions { half, .. } => Some(*half),
            _ => None,
        }
    }

    #[test]
    fn split_comments_render_in_the_half_they_were_left_on() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // rows[4..7] are carol's LEFT thread, rows[7..12] the RIGHT one; each
        // row of a thread carries the side it was anchored to.
        let halves: Vec<Option<Option<CommentSide>>> =
            rows[4..12].iter().map(row_half).collect();
        assert_eq!(
            halves,
            vec![
                Some(Some(CommentSide::Left)),  // carol header
                Some(Some(CommentSide::Left)),  // body
                Some(Some(CommentSide::Left)),  // actions
                Some(Some(CommentSide::Right)), // alice header
                Some(Some(CommentSide::Right)), // body
                Some(Some(CommentSide::Right)), // bob reply header
                Some(Some(CommentSide::Right)), // body
                Some(Some(CommentSide::Right)), // actions
            ]
        );
    }

    #[test]
    fn only_a_threads_first_row_draws_the_card_top() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // Exactly one top edge per thread, and it's the root's header — a
        // reply's header sits mid-card and must not cap it.
        let mut tops = 0;
        let mut threads = 0;
        for row in &rows {
            match row {
                Row::CommentHeader { top, is_reply, .. } => {
                    if *top {
                        tops += 1;
                        assert!(!is_reply, "a reply header must not open the card");
                    }
                }
                // The actions row always closes a thread, so it counts them.
                Row::CommentActions { .. } => threads += 1,
                _ => {}
            }
        }
        assert!(threads > 0, "fixture should have threads");
        assert_eq!(tops, threads, "one top edge per thread");
    }

    #[test]
    fn zoom_keybindings_parse() {
        // These strings are only parsed when the app boots, so a typo would be
        // a runtime panic rather than a compile error.
        for keys in ["cmd-=", "cmd-+", "cmd--", "cmd-0"] {
            assert!(
                Keystroke::parse(keys).is_ok(),
                "{keys:?} should be a valid keystroke"
            );
        }
    }

    #[test]
    fn row_height_follows_the_font_size() {
        // The default must reproduce the pre-zoom geometry exactly.
        assert_eq!(DEFAULT_TEXT_SIZE, 13.0);
        assert_eq!(row_height_for(DEFAULT_TEXT_SIZE), 22.0);
        // Rows grow and shrink with the text, always leaving headroom so
        // glyphs can't outgrow their row at either bound.
        assert_eq!(row_height_for(20.), 34.0);
        assert_eq!(row_height_for(8.), 14.0);
        for size in [MIN_TEXT_SIZE, DEFAULT_TEXT_SIZE, MAX_TEXT_SIZE] {
            assert!(
                row_height_for(size) > size,
                "row must be taller than {size}px text"
            );
        }
    }

    #[test]
    fn comment_bodies_wrap_to_the_requested_column_width() {
        // One long prose line plus an unbreakable token (a URL), the two ways a
        // body runs past the card.
        let body = format!("{} https://example.com/{}", "word ".repeat(60), "x".repeat(200));
        let index = group_comments(vec![rc(
            1,
            "a.rs",
            Some("RIGHT"),
            Some(2),
            &body,
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        for (name, mode, cap) in [
            ("unified", ViewMode::Unified, COMMENT_WRAP_CHARS),
            ("split", ViewMode::Split, 40),
        ] {
            let (rows, _, _) =
                build_rows(&sample_diff(), mode, &HashMap::new(), Some(&index), true, cap);
            let bodies: Vec<&SharedString> = rows
                .iter()
                .filter_map(|row| match row {
                    Row::CommentBody { line, .. } => Some(line),
                    _ => None,
                })
                .collect();
            assert!(!bodies.is_empty(), "{name} should have body rows");
            for line in bodies {
                assert!(
                    line.chars().count() <= cap,
                    "{name}: {:?} exceeds {cap} cols",
                    line.as_ref()
                );
            }
        }
    }

    #[test]
    fn unified_comments_span_the_full_width() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // Unified has one column, so no comment row is confined to a half.
        assert!(rows.iter().filter_map(row_half).all(|half| half.is_none()));
        // ...and the diff really does contain comment rows to check.
        assert!(rows.iter().any(is_comment_row));
    }

    #[test]
    fn hidden_comments_keep_header_counts() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            false,
                COMMENT_WRAP_CHARS,
        );
        assert!(!rows.iter().any(is_comment_row));
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (3, 1));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
        // And with no index at all (local items): zero counts, no rows.
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
                COMMENT_WRAP_CHARS,
        );
        assert!(!rows.iter().any(is_comment_row));
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (0, 0));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
    }

    #[test]
    fn expanded_gap_context_rows_host_threads() {
        let (diff, mut upgrades) = upgraded_diff();
        // a.txt line 3 lives in the leading gap (hunk covers 7..=13).
        let index = group_comments(vec![rc(
            9,
            "a.txt",
            Some("RIGHT"),
            Some(3),
            "gap comment",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let (rows, _, _) = build_rows(&diff, ViewMode::Unified, &upgrades, Some(&index), true, COMMENT_WRAP_CHARS);
        // Collapsed gap: the thread has no anchor row and stays hidden.
        assert!(!rows.iter().any(is_comment_row));
        upgrades.get_mut(&0).unwrap().expanded.insert(0);
        let (rows, _, _) = build_rows(&diff, ViewMode::Unified, &upgrades, Some(&index), true, COMMENT_WRAP_CHARS);
        // FileHeader, ctx 1, ctx 2, ctx 3, then the thread.
        assert_eq!(row_name(&rows[3]), "Line");
        assert_eq!(row_name(&rows[4]), "CommentHeader");
        assert_eq!(row_name(&rows[5]), "CommentBody");
        assert_eq!(row_name(&rows[6]), "CommentActions");
    }

    #[test]
    fn comment_anchor_resolves_sides_and_skips_non_lines() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // Headers and comment rows anchor nothing.
        assert_eq!(comment_anchor(&rows, 0, SelSide::Unified), None);
        assert_eq!(comment_anchor(&rows, 4, SelSide::Unified), None);
        // Context row (1,1) → RIGHT 1; removed old1 → LEFT 2; added new1 → RIGHT 2.
        assert_eq!(
            comment_anchor(&rows, 2, SelSide::Unified),
            Some((CommentSide::Right, 1))
        );
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Unified),
            Some((CommentSide::Left, 2))
        );
        assert_eq!(
            comment_anchor(&rows, 8, SelSide::Unified),
            Some((CommentSide::Right, 2))
        );
        // Split: the half under the pointer decides; absent cells refuse.
        let (rows, _, hunk_rows) =
            build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Left),
            Some((CommentSide::Left, 2))
        );
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Right),
            Some((CommentSide::Right, 2))
        );
        // Second hunk: r2 has no right cell (see split tests above).
        let h2 = hunk_rows[1];
        assert_eq!(comment_anchor(&rows, h2 + 2, SelSide::Right), None);
        assert_eq!(
            comment_anchor(&rows, h2 + 2, SelSide::Left),
            Some((CommentSide::Left, 11))
        );
    }

    #[test]
    fn nth_noncomment_row_maps_positions_across_comment_insertions() {
        let index = sample_comments();
        let (plain, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
                COMMENT_WRAP_CHARS,
        );
        let (with, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // Every plain row maps to the same row content with comments shown.
        for (n, row) in plain.iter().enumerate() {
            let ix = nth_noncomment_row(&with, n);
            assert_eq!(row_name(&with[ix]), row_name(row), "row {n}");
        }
        // n beyond the end clamps to the last row.
        assert_eq!(nth_noncomment_row(&plain, 999), plain.len() - 1);
    }

    // --- Chat with Claude ----------------------------------------------------

    #[test]
    fn chat_prompt_first_message_carries_header_patch_and_selection() {
        let prompt = chat_prompt(
            Some(("HEADER", "diff --git a/x b/x\n+new line\n")),
            Some("Selected text (x:1-1, RIGHT (new) side):\n```\nnew line\n```\n"),
            "why?",
        );
        assert!(prompt.starts_with("HEADER\n\nThe unified diff under review:\n```diff\n"));
        assert!(prompt.contains("+new line\n```\n"));
        assert!(!prompt.contains("truncated"));
        assert!(prompt.contains("Selected text (x:1-1"));
        assert!(prompt.ends_with("why?"));
        // Later turns: no context block, just selection (if any) + question.
        let followup = chat_prompt(None, None, "and this?");
        assert_eq!(followup, "and this?");
    }

    #[test]
    fn chat_prompt_truncates_patch_at_cap_on_char_boundary() {
        // 'a' then 2-byte 'é's: char boundaries sit at odd offsets, so the
        // even cap falls mid-char and must be shaved back to a boundary.
        let patch = format!("a{}", "é".repeat(MAX_CHAT_PATCH_BYTES));
        let prompt = chat_prompt(Some(("H", &patch)), None, "q");
        assert!(prompt.contains("(patch truncated at 200KB"));
        let (cut, truncated) = truncate_str(&patch, MAX_CHAT_PATCH_BYTES);
        assert!(truncated);
        assert_eq!(cut.len(), MAX_CHAT_PATCH_BYTES - 1); // boundary shaved one byte
        assert!(cut.is_char_boundary(cut.len()));
        let (all, truncated) = truncate_str("abc", 10);
        assert_eq!((all, truncated), ("abc", false));
    }

    #[test]
    fn chat_headers_describe_the_item() {
        let meta = gh::PrMeta {
            number: 7,
            title: "Fix the frobnicator".into(),
            author: gh::Author {
                login: "alice".into(),
            },
            state: "OPEN".into(),
            is_draft: false,
            url: "https://github.com/o/r/pull/7".into(),
            body: "It was broken.\n".into(),
            base_ref_name: "main".into(),
            head_ref_name: "fix".into(),
            base_ref_oid: String::new(),
            head_ref_oid: String::new(),
            additions: 1,
            deletions: 2,
            changed_files: 3,
            review_decision: String::new(),
            status_check_rollup: Vec::new(),
        };
        let header = pr_chat_header(&meta);
        assert!(header.contains("\"Fix the frobnicator\""));
        assert!(header.contains("https://github.com/o/r/pull/7"));
        assert!(header.contains("alice"));
        assert!(header.contains("OPEN"));
        assert!(header.contains("main ← fix"));
        assert!(header.contains("It was broken."));
        // Empty body → explicit placeholder, so the model doesn't guess.
        let meta = gh::PrMeta {
            body: "  \n".into(),
            ..meta
        };
        assert!(pr_chat_header(&meta).contains("(no description)"));

        let src = git::LocalSource {
            repo_root: "/tmp/myrepo".into(),
            branch: "feature".into(),
            base_ref: None,
            base_label: "origin/main".into(),
            base_oid: None,
        };
        let header = local_chat_header(&src);
        assert!(header.contains("myrepo"));
        assert!(header.contains("feature"));
        assert!(header.contains("origin/main"));
    }

    #[test]
    fn local_review_prompt_matches_difit_comment_format() {
        let src = git::LocalSource {
            repo_root: "/tmp/myrepo".into(),
            branch: "feature".into(),
            base_ref: None,
            base_label: "upstream/main".into(),
            base_oid: None,
        };
        let mut review = LocalReview::default();
        review.add_comment(
            None,
            "src/lib.rs".to_string(),
            CommentSide::Right,
            7,
            None,
            "why remove this?\nplease explain".to_string(),
        );
        review.add_comment(
            Some(1),
            "src/lib.rs".to_string(),
            CommentSide::Right,
            7,
            None,
            "because this path handles nil".to_string(),
        );
        review.add_comment(
            None,
            "src/main.rs".to_string(),
            CommentSide::Left,
            12,
            None,
            "second thread".to_string(),
        );

        assert_eq!(
            local_review_prompt(&src, &review),
            "Diff: upstream/main ← feature. Locations are GitHub diff-style: path:Rline for right/new, path:Lline for left/old.\n\nsrc/lib.rs:R7\nwhy remove this?\nplease explain\nReply 1 (you)\nbecause this path handles nil\n=====\nsrc/main.rs:L12\nsecond thread"
        );
    }

    #[test]
    fn scratch_paths_stay_inside_the_root() {
        let root = Path::new("/tmp/lgtm-chat-1-2");
        assert_eq!(
            scratch_path(root, "src/main.rs"),
            Some(root.join("src/main.rs"))
        );
        assert_eq!(
            scratch_path(root, "deep/a/b/c.txt"),
            Some(root.join("deep/a/b/c.txt"))
        );
        // Escapes and non-normal components are refused.
        assert_eq!(scratch_path(root, "../evil"), None);
        assert_eq!(scratch_path(root, "a/../../evil"), None);
        assert_eq!(scratch_path(root, "/abs/path"), None);
        assert_eq!(scratch_path(root, "./x"), None);
        assert_eq!(scratch_path(root, ""), None);
    }

    #[test]
    fn materialize_writes_files_and_skips_oversized_and_unsafe() {
        let root = std::env::temp_dir().join(format!(
            "lgtm-chat-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let files = vec![
            ("src/lib.rs".to_string(), "fn a() {}\n".to_string()),
            ("../escape.rs".to_string(), "nope\n".to_string()),
            ("big.rs".to_string(), "x".repeat(MAX_EXPLORE_FILE_BYTES + 1)),
        ];
        let dir = materialize_files(&root, &files).unwrap();
        assert_eq!(dir, root);
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "fn a() {}\n"
        );
        assert!(!root.join("big.rs").exists());
        assert!(!root.parent().unwrap().join("escape.rs").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_selection_wins_over_intra() {
        let bg = Some(theme::added_word_bg());
        let sel_style = |token: Option<Token>| {
            let mut style = token.map(style).unwrap_or_default();
            style.background_color = Some(theme::selection_bg().into());
            style
        };
        // Intra 2..6, selection 4..8: the overlap 4..6 paints selection bg.
        assert_eq!(
            merge_highlights(&[], &[2..6], bg, Some(4..8)),
            vec![(2..4, style_bg(None)), (4..8, sel_style(None))]
        );
        // Selection over a syntax token keeps the token foreground.
        let syntax = [(0..4, Token::Keyword)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, Some(2..6)),
            vec![
                (0..2, style(Token::Keyword)),
                (2..4, sel_style(Some(Token::Keyword))),
                (4..6, sel_style(None)),
            ]
        );
    }
}
