//! Fetch PR data via the `gh` CLI, piggybacking on the user's `gh auth`.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct PrLocator {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PrLocator {
    pub fn repo_slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Accepts `owner/repo#123`, `#123`, `123`, or a GitHub PR URL. Bare numbers
/// resolve the repo from the current directory's git remote (via `gh`).
pub fn resolve_pr_arg(arg: &str) -> Result<PrLocator> {
    if let Some(rest) = arg
        .strip_prefix("https://github.com/")
        .or_else(|| arg.strip_prefix("http://github.com/"))
        .or_else(|| arg.strip_prefix("github.com/"))
    {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 4 && parts[2] == "pull" {
            let digits: String = parts[3].chars().take_while(char::is_ascii_digit).collect();
            let number = digits
                .parse()
                .with_context(|| format!("no PR number in URL {arg}"))?;
            return Ok(PrLocator {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
                number,
            });
        }
        bail!("unrecognized GitHub URL: {arg}");
    }

    if let Some((repo_part, number)) = arg.split_once('#') {
        let number = number
            .parse()
            .with_context(|| format!("invalid PR number in {arg}"))?;
        if repo_part.is_empty() {
            return locator_in_cwd_repo(number);
        }
        let (owner, repo) = repo_part
            .split_once('/')
            .with_context(|| format!("expected owner/repo before '#' in {arg}"))?;
        return Ok(PrLocator {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        });
    }

    if let Ok(number) = arg.parse() {
        return locator_in_cwd_repo(number);
    }

    bail!("could not parse {arg:?}; expected owner/repo#123, a PR URL, or a PR number")
}

fn locator_in_cwd_repo(number: u64) -> Result<PrLocator> {
    let out = gh(&[
        "repo",
        "view",
        "--json",
        "nameWithOwner",
        "--jq",
        ".nameWithOwner",
    ])
    .context("couldn't infer the repo from the current directory; use owner/repo#123")?;
    let (owner, repo) = out
        .trim()
        .split_once('/')
        .with_context(|| format!("unexpected gh repo view output: {out}"))?;
    Ok(PrLocator {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    })
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrMeta {
    pub number: u64,
    pub title: String,
    pub author: Author,
    pub state: String,
    pub is_draft: bool,
    pub url: String,
    /// PR description (markdown); empty when the PR has none. Used as chat
    /// context, not rendered in the UI.
    #[serde(default)]
    pub body: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    #[serde(default)]
    pub base_ref_oid: String,
    #[serde(default)]
    pub head_ref_oid: String,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    /// "APPROVED", "CHANGES_REQUESTED", "REVIEW_REQUIRED", or "" (no
    /// required reviews and none given).
    #[serde(default)]
    pub review_decision: String,
    #[serde(default)]
    pub status_check_rollup: Vec<CheckRun>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Author {
    pub login: String,
}

/// One entry of `statusCheckRollup`. GitHub mixes two shapes in this array:
/// legacy commit statuses report `state`, GitHub Actions check runs report
/// `status`/`conclusion`. Absent fields default to empty so either shape
/// deserializes into the same struct.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CheckRun {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub conclusion: String,
}

impl CheckRun {
    pub fn passed(&self) -> bool {
        matches!(self.state.as_str(), "SUCCESS" | "EXPECTED")
            || matches!(self.conclusion.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED")
    }

    fn failed(&self) -> bool {
        matches!(self.state.as_str(), "FAILURE" | "ERROR")
            || matches!(
                self.conclusion.as_str(),
                "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
            )
    }
}

/// Overall CI state for a PR's `statusCheckRollup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiState {
    Passed,
    InProgress,
    Failed,
}

/// Summarizes a PR's CI checks as a passed/total count plus an overall
/// state, or `None` when the PR has no checks at all.
pub fn ci_summary(checks: &[CheckRun]) -> Option<(usize, usize, CiState)> {
    if checks.is_empty() {
        return None;
    }
    let total = checks.len();
    let passed = checks.iter().filter(|c| c.passed()).count();
    let state = if checks.iter().any(CheckRun::failed) {
        CiState::Failed
    } else if passed == total {
        CiState::Passed
    } else {
        CiState::InProgress
    };
    Some((passed, total, state))
}

pub fn fetch_meta(loc: &PrLocator) -> Result<PrMeta> {
    let json = gh(&[
        "pr",
        "view",
        &loc.number.to_string(),
        "--repo",
        &loc.repo_slug(),
        "--json",
        "number,title,author,state,isDraft,url,body,baseRefName,headRefName,baseRefOid,\
         headRefOid,additions,deletions,changedFiles,reviewDecision,statusCheckRollup",
    ])?;
    serde_json::from_str(&json).context("unexpected gh pr view JSON")
}

/// One row of `gh pr list` output, for the PR picker.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub author: Author,
    pub state: String,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub updated_at: String,
    #[serde(default)]
    pub review_decision: String,
    #[serde(default)]
    pub status_check_rollup: Vec<CheckRun>,
}

/// Open PRs for a repo, most recently updated first (gh's default order).
pub fn list_prs(owner: &str, repo: &str) -> Result<Vec<PrSummary>> {
    let json = gh(&[
        "pr",
        "list",
        "--repo",
        &format!("{owner}/{repo}"),
        "--state",
        "open",
        "--limit",
        "200",
        "--json",
        "number,title,author,state,isDraft,headRefName,updatedAt,reviewDecision,\
         statusCheckRollup",
    ])?;
    serde_json::from_str(&json).context("unexpected gh pr list JSON")
}

/// One PR review comment from the REST pulls/comments API. Unlike `gh pr
/// view --json` (camelCase), this endpoint returns snake_case field names, so
/// no `rename_all` here.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub path: String,
    /// Anchor line on `side` against the *current* diff; None = outdated
    /// (the code the comment was written against has since changed).
    pub line: Option<u64>,
    /// "RIGHT" (new side) or "LEFT" (old side).
    pub side: Option<String>,
    /// Multi-line comments start here and anchor at `line` (the end), like
    /// GitHub's own UI.
    pub start_line: Option<u64>,
    pub body: String,
    pub user: Author,
    pub created_at: String,
    pub in_reply_to_id: Option<u64>,
}

/// Every review comment on the PR. `--paginate --slurp` (gh ≥ 2.66; we
/// require it) wraps each page's JSON array into one array-of-arrays —
/// without `--slurp`, `--paginate` concatenates the arrays back-to-back
/// ("[…][…]"), which serde can't parse.
pub fn fetch_review_comments(loc: &PrLocator) -> Result<Vec<ReviewComment>> {
    let json = gh(&[
        "api",
        "--paginate",
        "--slurp",
        &format!(
            "repos/{}/{}/pulls/{}/comments?per_page=100",
            loc.owner, loc.repo, loc.number
        ),
    ])?;
    let pages: Vec<Vec<ReviewComment>> =
        serde_json::from_str(&json).context("unexpected gh pulls/comments JSON")?;
    Ok(pages.into_iter().flatten().collect())
}

/// One top-level PR comment as the REST `issues/comments` endpoint returns
/// it: `snake_case`, with the author under `user` (the GraphQL path used for
/// issues calls the same field `author`). Mapped into [`IssueComment`].
#[derive(Debug, Clone, serde::Deserialize)]
struct RawPrIssueComment {
    user: Author,
    body: String,
    created_at: String,
}

/// The PR's top-level conversation comments, oldest first. PRs are issues in
/// GitHub's API, so these come from the issues endpoint, not `pulls`; same
/// `--paginate --slurp` shape as [`fetch_review_comments`].
pub fn fetch_pr_comments(loc: &PrLocator) -> Result<Vec<IssueComment>> {
    let json = gh(&[
        "api",
        "--paginate",
        "--slurp",
        &format!(
            "repos/{}/{}/issues/{}/comments?per_page=100",
            loc.owner, loc.repo, loc.number
        ),
    ])?;
    let pages: Vec<Vec<RawPrIssueComment>> =
        serde_json::from_str(&json).context("unexpected gh issues/comments JSON")?;
    Ok(pages
        .into_iter()
        .flatten()
        .map(|raw| IssueComment {
            author: raw.user.login,
            body: raw.body,
            created_at: raw.created_at,
        })
        .collect())
}

/// One review on the PR: the verdict plus the summary body the reviewer left
/// with it (empty when they only wrote inline comments).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PrReview {
    pub id: u64,
    pub user: Author,
    pub body: String,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`, or
    /// `PENDING` (the viewer's own review, not yet submitted).
    pub state: String,
    /// Absent on `PENDING` reviews, which were never submitted.
    pub submitted_at: Option<String>,
}

/// Every review on the PR, in submission order.
pub fn fetch_pr_reviews(loc: &PrLocator) -> Result<Vec<PrReview>> {
    let json = gh(&[
        "api",
        "--paginate",
        "--slurp",
        &format!(
            "repos/{}/{}/pulls/{}/reviews?per_page=100",
            loc.owner, loc.repo, loc.number
        ),
    ])?;
    let pages: Vec<Vec<PrReview>> =
        serde_json::from_str(&json).context("unexpected gh pulls/reviews JSON")?;
    Ok(pages.into_iter().flatten().collect())
}

/// A user who can be @-mentioned on this repo, for comment autocomplete.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Mention {
    pub login: String,
    /// The user's display name, if set (shown as a hint beside the login).
    #[serde(default)]
    pub name: Option<String>,
}

/// Never page past this many mentionable users; big repos can have thousands,
/// and the autocomplete only needs a workable pool of the most relevant.
const MAX_MENTIONABLE: usize = 500;

/// Users who can be @-mentioned on the PR's repo, via the GraphQL
/// `mentionableUsers` connection (the same set GitHub's own comment box
/// autocompletes from). Paginated up to `MAX_MENTIONABLE`.
pub fn fetch_mentionable_users(loc: &PrLocator) -> Result<Vec<Mention>> {
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Data,
    }
    #[derive(serde::Deserialize)]
    struct Data {
        repository: RepositoryField,
    }
    #[derive(serde::Deserialize)]
    struct RepositoryField {
        #[serde(rename = "mentionableUsers")]
        mentionable_users: Connection,
    }
    #[derive(serde::Deserialize)]
    struct Connection {
        nodes: Vec<Mention>,
        #[serde(rename = "pageInfo")]
        page_info: PageInfo,
    }
    #[derive(serde::Deserialize)]
    struct PageInfo {
        #[serde(rename = "hasNextPage")]
        has_next_page: bool,
        #[serde(rename = "endCursor")]
        end_cursor: Option<String>,
    }

    const QUERY: &str = "query($owner:String!,$repo:String!,$after:String){\
        repository(owner:$owner,name:$repo){\
        mentionableUsers(first:100,after:$after){\
        nodes{login name} pageInfo{hasNextPage endCursor}}}}";

    let mut users = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={QUERY}"),
            "-f".to_string(),
            format!("owner={}", loc.owner),
            "-f".to_string(),
            format!("repo={}", loc.repo),
        ];
        if let Some(after) = &cursor {
            args.push("-f".to_string());
            args.push(format!("after={after}"));
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let json = gh(&arg_refs)?;
        let resp: Resp =
            serde_json::from_str(&json).context("unexpected gh graphql mentionableUsers JSON")?;
        let conn = resp.data.repository.mentionable_users;
        users.extend(conn.nodes);
        if users.len() >= MAX_MENTIONABLE || !conn.page_info.has_next_page {
            break;
        }
        match conn.page_info.end_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(users)
}

/// Post a new top-level review comment anchored at (path, side, line) against
/// `commit_id` (the PR's head oid). Errors carry gh's stderr — a 403 usually
/// means a missing token scope, a 422 an unanchorable line.
pub fn post_review_comment(
    loc: &PrLocator,
    commit_id: &str,
    path: &str,
    side: &str,
    line: u64,
    start_line: Option<u64>,
    body: &str,
) -> Result<()> {
    let mut args = vec![
        "api".to_string(),
        "-X".to_string(),
        "POST".to_string(),
        format!(
            "repos/{}/{}/pulls/{}/comments",
            loc.owner, loc.repo, loc.number
        ),
        "-f".to_string(),
        format!("body={body}"),
        "-f".to_string(),
        format!("commit_id={commit_id}"),
        "-f".to_string(),
        format!("path={path}"),
        "-f".to_string(),
        format!("side={side}"),
        // -F, not -f: line must be a JSON integer, not a string.
        "-F".to_string(),
        format!("line={line}"),
    ];
    // A multi-line comment additionally anchors its start on the same side
    // (GitHub doesn't support a range spanning left and right).
    if let Some(start_line) = start_line {
        args.push("-F".to_string());
        args.push(format!("start_line={start_line}"));
        args.push("-f".to_string());
        args.push(format!("start_side={side}"));
    }
    gh(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    Ok(())
}

/// The verdict of a top-level PR review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Comment,
}

/// Submit a top-level review on the PR. GitHub requires a non-empty body for
/// request-changes and comment reviews (the caller enforces this so the
/// error surfaces before a network round-trip); approvals may be bodyless.
pub fn submit_review(loc: &PrLocator, verdict: ReviewVerdict, body: &str) -> Result<()> {
    let number = loc.number.to_string();
    let slug = loc.repo_slug();
    let flag = match verdict {
        ReviewVerdict::Approve => "--approve",
        ReviewVerdict::RequestChanges => "--request-changes",
        ReviewVerdict::Comment => "--comment",
    };
    let mut args = vec!["pr", "review", &number, "--repo", &slug, flag];
    if !body.is_empty() {
        args.push("--body");
        args.push(body);
    }
    gh(&args)?;
    Ok(())
}

/// Reply to the review thread rooted at `comment_id`.
pub fn post_reply(loc: &PrLocator, comment_id: u64, body: &str) -> Result<()> {
    gh(&[
        "api",
        "-X",
        "POST",
        &format!(
            "repos/{}/{}/pulls/{}/comments/{}/replies",
            loc.owner, loc.repo, loc.number, comment_id
        ),
        "-f",
        &format!("body={body}"),
    ])?;
    Ok(())
}

pub fn fetch_patch(loc: &PrLocator) -> Result<String> {
    gh(&[
        "pr",
        "diff",
        &loc.number.to_string(),
        "--repo",
        &loc.repo_slug(),
    ])
}

/// Blob-size cap: PR review never needs multi-megabyte files, and the raw
/// contents API happily serves up to 100 MB.
const MAX_BLOB_BYTES: usize = 1024 * 1024;

/// Full contents of `path` at `commit_oid`, via the raw contents API.
/// `Ok(None)` means "leave this file un-upgraded": absent on that side (404),
/// non-UTF-8 (binary), or larger than [`MAX_BLOB_BYTES`]. A path at a commit
/// is immutable, so results — including negative ones — are cached on disk in
/// `~/.cache/lgtm/blobs/` (sha256 of `repo\0oid\0path`, with an `.absent`
/// sidecar marking negative entries).
pub fn fetch_file_at(loc: &PrLocator, commit_oid: &str, path: &str) -> Result<Option<String>> {
    let cache = cache_path(&loc.repo_slug(), commit_oid, path);
    if let Some(cache) = &cache {
        if cache.with_extension("absent").exists() {
            return Ok(None);
        }
        if let Ok(bytes) = std::fs::read(cache) {
            return Ok(String::from_utf8(bytes).ok());
        }
    }

    let endpoint = format!(
        "repos/{}/{}/contents/{}?ref={}",
        loc.owner,
        loc.repo,
        encode_path(path),
        commit_oid
    );
    let output = Command::new("gh")
        .args([
            "api",
            "-H",
            "Accept: application/vnd.github.raw+json",
            &endpoint,
        ])
        .output()
        .map_err(|err| anyhow!("failed to run gh (is the GitHub CLI installed?): {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("404") {
            mark_absent(cache);
            return Ok(None);
        }
        bail!("gh api {endpoint} failed: {}", stderr.trim());
    }
    if output.stdout.len() > MAX_BLOB_BYTES {
        mark_absent(cache);
        return Ok(None);
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        mark_absent(cache);
        return Ok(None);
    };
    if let Some(cache) = cache {
        let _ = std::fs::write(cache, &text);
    }
    Ok(Some(text))
}

fn mark_absent(cache: Option<PathBuf>) {
    if let Some(cache) = cache {
        let _ = std::fs::write(cache.with_extension("absent"), b"");
    }
}

/// `~/.cache/lgtm/blobs/<key>`, creating the directory; None when HOME is
/// unset or the directory can't be created (cache disabled, fetch still works).
fn cache_path(repo: &str, oid: &str, path: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HOME")?)
        .join(".cache")
        .join("lgtm")
        .join("blobs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(cache_key(repo, oid, path)))
}

/// Stable cache key: hex sha256 of `repo\0oid\0path`.
pub fn cache_key(repo: &str, oid: &str, path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo.as_bytes());
    hasher.update([0]);
    hasher.update(oid.as_bytes());
    hasher.update([0]);
    hasher.update(path.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Percent-encode a repo path for the contents API, keeping `/` separators:
/// every byte outside RFC 3986 unreserved is encoded, per segment.
pub fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// --- Tracker: Projects v2 boards, fields, items, issue detail, writes ---

/// One Projects v2 board, as returned by the `projectsV2` connection on a
/// repository.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProjectBoard {
    pub id: String,
    pub number: u64,
    pub title: String,
}

/// Projects v2 boards linked to `owner/repo`, in the order GitHub returns
/// them (the tracker uses the first one).
pub fn list_project_boards(owner: &str, repo: &str) -> Result<Vec<ProjectBoard>> {
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Data,
    }
    #[derive(serde::Deserialize)]
    struct Data {
        repository: RepositoryField,
    }
    #[derive(serde::Deserialize)]
    struct RepositoryField {
        #[serde(rename = "projectsV2")]
        projects_v2: Connection,
    }
    #[derive(serde::Deserialize)]
    struct Connection {
        nodes: Vec<ProjectBoard>,
    }

    const QUERY: &str = "query($owner:String!,$repo:String!){\
        repository(owner:$owner,name:$repo){\
        projectsV2(first:10){nodes{id number title}}}}";

    let json = gh(&[
        "api",
        "graphql",
        "-f",
        &format!("query={QUERY}"),
        "-f",
        &format!("owner={owner}"),
        "-f",
        &format!("repo={repo}"),
    ])?;
    let resp: Resp =
        serde_json::from_str(&json).context("unexpected gh graphql projectsV2 JSON")?;
    Ok(resp.data.repository.projects_v2.nodes)
}

/// A single-select field's option, in board order.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ProjectFieldOption {
    pub id: String,
    pub name: String,
}

/// A project field. `options` is empty for non-single-select fields (Title,
/// text, date, ...).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ProjectField {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub options: Vec<ProjectFieldOption>,
}

/// Fields on project `number` (owned by `owner`), via `gh project field-list`.
pub fn project_fields(owner: &str, number: u64) -> Result<Vec<ProjectField>> {
    #[derive(serde::Deserialize)]
    struct Resp {
        fields: Vec<ProjectField>,
    }
    let json = gh(&[
        "project",
        "field-list",
        &number.to_string(),
        "--owner",
        owner,
        "--format",
        "json",
    ])?;
    let resp: Resp =
        serde_json::from_str(&json).context("unexpected gh project field-list JSON")?;
    Ok(resp.fields)
}

/// The `content` an item wraps: an issue, a PR, or a draft issue.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ProjectItemContent {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub number: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub repository: String,
}

/// One row of `gh project item-list`, with the Status/Due/Priority fields
/// the tracker board is expected to have (empty string when unset).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ProjectItem {
    pub id: String,
    #[serde(default)]
    pub content: ProjectItemContent,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub due: String,
    #[serde(default)]
    pub priority: String,
}

/// Never page past this many items in one tracker refresh.
const MAX_PROJECT_ITEMS: &str = "500";

/// Items on project `number`, filtered to issues (drafts and PRs dropped).
pub fn project_items(owner: &str, number: u64) -> Result<Vec<ProjectItem>> {
    #[derive(serde::Deserialize)]
    struct Resp {
        items: Vec<ProjectItem>,
    }
    let json = gh(&[
        "project",
        "item-list",
        &number.to_string(),
        "--owner",
        owner,
        "--format",
        "json",
        "--limit",
        MAX_PROJECT_ITEMS,
    ])?;
    let resp: Resp = serde_json::from_str(&json).context("unexpected gh project item-list JSON")?;
    Ok(resp
        .items
        .into_iter()
        .filter(|item| item.content.kind == "Issue")
        .collect())
}

/// Set a single-select field on a project item (e.g. Status, Priority) via
/// `gh project item-edit`. `project_id` and `item_id` are Projects v2 node
/// ids, not the issue number.
pub fn set_project_field(project_id: &str, item_id: &str, field_id: &str, option_id: &str) -> Result<()> {
    gh(&[
        "project",
        "item-edit",
        "--id",
        item_id,
        "--project-id",
        project_id,
        "--field-id",
        field_id,
        "--single-select-option-id",
        option_id,
    ])?;
    Ok(())
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct RawLogin {
    login: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct NodesConnection<T> {
    #[serde(default)]
    nodes: Vec<T>,
}

/// A label on an issue.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct IssueLabel {
    pub name: String,
    pub color: String,
}

/// An issue's milestone, if any.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueMilestone {
    pub title: String,
    #[serde(default)]
    pub due_on: Option<String>,
}

/// A sub-issue, or the issue's own row in `subIssues`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SubIssue {
    pub number: u64,
    pub title: String,
    pub state: String,
}

/// Sub-issue completion counts, straight from `subIssuesSummary`.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
pub struct SubIssuesSummary {
    pub total: u64,
    pub completed: u64,
}

/// The issue this one is a sub-issue of, if any.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ParentIssue {
    pub number: u64,
    pub title: String,
}

/// A PR linked to the issue via `closedByPullRequestsReferences`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedPr {
    pub number: u64,
    pub state: String,
    #[serde(default)]
    pub is_draft: bool,
    pub url: String,
}

/// One issue comment.
#[derive(Debug, Clone, Default)]
pub struct IssueComment {
    pub author: String,
    pub body: String,
    pub created_at: String,
}

/// Full detail for one issue: everything the tracker's issue panel shows.
#[derive(Debug, Clone, Default)]
pub struct IssueDetail {
    pub title: String,
    pub body: String,
    pub state: String,
    pub url: String,
    pub created_at: String,
    pub assignees: Vec<String>,
    pub labels: Vec<IssueLabel>,
    pub milestone: Option<IssueMilestone>,
    /// GitHub issue type name (Bug, Feature, ...), when the org has them
    /// enabled; absent otherwise.
    pub issue_type: Option<String>,
    pub sub_issues: Vec<SubIssue>,
    pub sub_issues_summary: SubIssuesSummary,
    pub parent: Option<ParentIssue>,
    pub linked_prs: Vec<LinkedPr>,
    pub comments: Vec<IssueComment>,
}

/// The GraphQL response shape, matching `ISSUE_DETAIL_QUERY` field for
/// field; converted into the flatter [`IssueDetail`] callers use.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIssueDetail {
    title: String,
    #[serde(default)]
    body: String,
    state: String,
    url: String,
    created_at: String,
    #[serde(default)]
    assignees: NodesConnection<RawLogin>,
    #[serde(default)]
    labels: NodesConnection<IssueLabel>,
    #[serde(default)]
    milestone: Option<IssueMilestone>,
    #[serde(default)]
    issue_type: Option<IssueTypeName>,
    #[serde(default)]
    sub_issues: NodesConnection<SubIssue>,
    #[serde(default)]
    sub_issues_summary: SubIssuesSummary,
    #[serde(default)]
    parent: Option<ParentIssue>,
    #[serde(default)]
    closed_by_pull_requests_references: NodesConnection<LinkedPr>,
    #[serde(default)]
    comments: NodesConnection<RawIssueComment>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct IssueTypeName {
    name: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawIssueComment {
    author: RawLogin,
    body: String,
    created_at: String,
}

impl From<RawIssueDetail> for IssueDetail {
    fn from(raw: RawIssueDetail) -> Self {
        IssueDetail {
            title: raw.title,
            body: raw.body,
            state: raw.state,
            url: raw.url,
            created_at: raw.created_at,
            assignees: raw.assignees.nodes.into_iter().map(|a| a.login).collect(),
            labels: raw.labels.nodes,
            milestone: raw.milestone,
            issue_type: raw.issue_type.map(|t| t.name),
            sub_issues: raw.sub_issues.nodes,
            sub_issues_summary: raw.sub_issues_summary,
            parent: raw.parent,
            linked_prs: raw.closed_by_pull_requests_references.nodes,
            comments: raw
                .comments
                .nodes
                .into_iter()
                .map(|c| IssueComment {
                    author: c.author.login,
                    body: c.body,
                    created_at: c.created_at,
                })
                .collect(),
        }
    }
}

const ISSUE_DETAIL_FIELDS: &str = "\
    title body state url createdAt \
    assignees(first:20){nodes{login}} \
    labels(first:20){nodes{name color}} \
    milestone{title dueOn} \
    issueType{name} \
    subIssues(first:50){nodes{number title state}} \
    subIssuesSummary{total completed} \
    parent{number title} \
    closedByPullRequestsReferences(first:20){nodes{number state isDraft url}} \
    comments(first:50){nodes{author{login} body createdAt}}";

/// Full detail for `owner/repo#number`: title, body, assignees, labels,
/// milestone, issue type, sub-issues (with summary), parent, linked PRs and
/// comments, in one GraphQL round trip. Blocked-by relations are not
/// included: GitHub's sub-issues GraphQL surface does not yet document a
/// stable `blockedBy` connection on `Issue`, and an unknown field fails the
/// whole query rather than degrading gracefully.
pub fn issue_detail(owner: &str, repo: &str, number: u64) -> Result<IssueDetail> {
    issue_details(owner, repo, &[number])?
        .pop()
        .map(|(_, detail)| detail)
        .with_context(|| format!("issue #{number} not found"))
}

/// [`issue_detail`] for many issues in one GraphQL request, one alias per
/// issue (`i114: issue(number:114){...}`). Issues GitHub returns as null
/// (deleted, transferred) are skipped. Keep batches to a few dozen: the
/// query grows linearly and GitHub caps request size and node count.
pub fn issue_details(owner: &str, repo: &str, numbers: &[u64]) -> Result<Vec<(u64, IssueDetail)>> {
    if numbers.is_empty() {
        return Ok(Vec::new());
    }
    let selections: String = numbers
        .iter()
        .map(|n| format!("i{n}:issue(number:{n}){{{ISSUE_DETAIL_FIELDS}}} "))
        .collect();
    let query =
        format!("query($owner:String!,$repo:String!){{repository(owner:$owner,name:$repo){{{selections}}}}}");
    let json = gh(&[
        "api",
        "graphql",
        "-f",
        &format!("query={query}"),
        "-f",
        &format!("owner={owner}"),
        "-f",
        &format!("repo={repo}"),
    ])?;
    parse_issue_details(&json, numbers)
}

fn parse_issue_details(json: &str, numbers: &[u64]) -> Result<Vec<(u64, IssueDetail)>> {
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Data,
    }
    #[derive(serde::Deserialize)]
    struct Data {
        repository: std::collections::BTreeMap<String, Option<RawIssueDetail>>,
    }

    let resp: Resp = serde_json::from_str(json).context("unexpected gh graphql issue JSON")?;
    Ok(numbers
        .iter()
        .filter_map(|n| {
            let raw = resp.data.repository.get(&format!("i{n}"))?.clone()?;
            Some((*n, raw.into()))
        })
        .collect())
}

/// Link `child_number` as a sub-issue of `parent_number`, via the REST
/// sub-issues endpoint. Looks up the child's numeric database id first
/// (the endpoint wants that, not the issue number).
pub fn add_sub_issue(owner: &str, repo: &str, parent_number: u64, child_number: u64) -> Result<()> {
    let child_id = gh(&[
        "api",
        &format!("repos/{owner}/{repo}/issues/{child_number}"),
        "--jq",
        ".id",
    ])?;
    gh(&[
        "api",
        "-X",
        "POST",
        &format!("repos/{owner}/{repo}/issues/{parent_number}/sub_issues"),
        "-F",
        &format!("sub_issue_id={}", child_id.trim()),
    ])?;
    Ok(())
}

/// Create an issue and return its number, parsed from `gh issue create`'s
/// printed URL.
pub fn create_issue(
    owner: &str,
    repo: &str,
    title: &str,
    body: &str,
    labels: &[String],
    milestone: Option<&str>,
) -> Result<u64> {
    let slug = format!("{owner}/{repo}");
    let label_arg = labels.join(",");
    let mut args = vec![
        "issue", "create", "--repo", &slug, "--title", title, "--body", body,
    ];
    if !labels.is_empty() {
        args.push("--label");
        args.push(&label_arg);
    }
    if let Some(milestone) = milestone {
        args.push("--milestone");
        args.push(milestone);
    }
    let out = gh(&args)?;
    let url = out.trim();
    url.rsplit('/')
        .next()
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("couldn't parse issue number from gh issue create output: {url}"))
}

/// Add an existing issue (by URL) to a project board.
pub fn add_item_to_project(owner: &str, project_number: u64, issue_url: &str) -> Result<()> {
    gh(&[
        "project",
        "item-add",
        &project_number.to_string(),
        "--owner",
        owner,
        "--url",
        issue_url,
    ])?;
    Ok(())
}

pub fn post_issue_comment(owner: &str, repo: &str, number: u64, body: &str) -> Result<()> {
    gh(&[
        "issue",
        "comment",
        &number.to_string(),
        "--repo",
        &format!("{owner}/{repo}"),
        "--body",
        body,
    ])?;
    Ok(())
}

pub fn update_issue_body(owner: &str, repo: &str, number: u64, body: &str) -> Result<()> {
    gh(&[
        "issue",
        "edit",
        &number.to_string(),
        "--repo",
        &format!("{owner}/{repo}"),
        "--body",
        body,
    ])?;
    Ok(())
}

pub fn update_issue_title(owner: &str, repo: &str, number: u64, title: &str) -> Result<()> {
    gh(&[
        "issue",
        "edit",
        &number.to_string(),
        "--repo",
        &format!("{owner}/{repo}"),
        "--title",
        title,
    ])?;
    Ok(())
}

/// The authenticated `gh` user's login, for the tracker's assignee filter.
pub fn current_user_login() -> Result<String> {
    let login = gh(&["api", "user", "--jq", ".login"])?;
    Ok(login.trim().to_string())
}

fn gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .map_err(|err| anyhow!("failed to run gh (is the GitHub CLI installed?): {err}"))?;
    if !output.status.success() {
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("gh output was not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_details_maps_aliases_and_skips_nulls() {
        let json = r#"{"data":{"repository":{
            "i7":{"title":"Seven","state":"OPEN","url":"u7","createdAt":"2026-01-01T00:00:00Z"},
            "i9":null,
            "i11":{"title":"Eleven","state":"OPEN","url":"u11","createdAt":"2026-01-01T00:00:00Z",
                   "labels":{"nodes":[{"name":"bug","color":"ff0000"}]}}
        }}}"#;
        let details = parse_issue_details(json, &[7, 9, 11]).unwrap();
        let numbers: Vec<u64> = details.iter().map(|(n, _)| *n).collect();
        assert_eq!(numbers, vec![7, 11]);
        assert_eq!(details[0].1.title, "Seven");
        assert_eq!(details[1].1.labels[0].name, "bug");
    }

    #[test]
    fn parses_slug_form() {
        let loc = resolve_pr_arg("zed-industries/zed#12345").unwrap();
        assert_eq!(loc.owner, "zed-industries");
        assert_eq!(loc.repo, "zed");
        assert_eq!(loc.number, 12345);
    }

    #[test]
    fn parses_url_form() {
        let loc = resolve_pr_arg("https://github.com/rust-lang/rust/pull/99999/files").unwrap();
        assert_eq!(loc.owner, "rust-lang");
        assert_eq!(loc.repo, "rust");
        assert_eq!(loc.number, 99999);
    }

    #[test]
    fn rejects_garbage() {
        assert!(resolve_pr_arg("not-a-pr").is_err());
    }

    #[test]
    fn check_run_passed_handles_both_status_shapes() {
        // Legacy commit status: `state`.
        let status_ctx = CheckRun {
            state: "SUCCESS".into(),
            conclusion: String::new(),
        };
        assert!(status_ctx.passed());

        // GitHub Actions check run: `conclusion`.
        let check_run = CheckRun {
            state: String::new(),
            conclusion: "NEUTRAL".into(),
        };
        assert!(check_run.passed());

        let failing = CheckRun {
            state: "FAILURE".into(),
            conclusion: String::new(),
        };
        assert!(!failing.passed());

        let pending = CheckRun {
            state: String::new(),
            conclusion: String::new(),
        };
        assert!(!pending.passed());
    }

    #[test]
    fn ci_summary_reflects_pass_fail_and_pending() {
        assert_eq!(ci_summary(&[]), None);

        let passed = CheckRun {
            state: "SUCCESS".into(),
            conclusion: String::new(),
        };
        let pending = CheckRun {
            state: String::new(),
            conclusion: String::new(),
        };
        let failing = CheckRun {
            state: "FAILURE".into(),
            conclusion: String::new(),
        };

        assert_eq!(
            ci_summary(&[passed.clone(), passed.clone()]),
            Some((2, 2, CiState::Passed))
        );
        assert_eq!(
            ci_summary(&[passed.clone(), pending.clone()]),
            Some((1, 2, CiState::InProgress))
        );
        assert_eq!(
            ci_summary(&[passed, pending, failing]),
            Some((1, 3, CiState::Failed))
        );
    }

    #[test]
    fn encodes_paths_per_segment_keeping_slashes() {
        assert_eq!(encode_path("src/main.rs"), "src/main.rs");
        assert_eq!(
            encode_path("dir with space/naïve+file#1.rs"),
            "dir%20with%20space/na%C3%AFve%2Bfile%231.rs"
        );
        assert_eq!(encode_path("a?b&c=d/e%f"), "a%3Fb%26c%3Dd/e%25f");
        assert_eq!(encode_path("A-Z_a.z~0/9"), "A-Z_a.z~0/9");
    }

    #[test]
    fn cache_key_is_stable() {
        // Pinned: changing this constant silently invalidates every user's
        // on-disk cache. The components are NUL-separated so `("a/b", "c")`
        // and `("a", "b/c")` can't collide.
        assert_eq!(
            cache_key(
                "BurntSushi/ripgrep",
                "f16ea0a8cfd0fbb0328b8348972356d532b921d0",
                "crates/core/main.rs"
            ),
            "1b21e76f45d6d948cf7b44c696608c64bdaeee714ac61b21dce21750ad9cb6bc"
        );
        assert_ne!(
            cache_key("o/r", "oid", "a/b"),
            cache_key("o/r/a", "oid", "b")
        );
    }

    #[test]
    fn deserializes_pr_meta_with_oids() {
        let json = r#"{
            "number": 1, "title": "t", "author": {"login": "a"}, "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/o/r/pull/1",
            "baseRefName": "main", "headRefName": "feat",
            "baseRefOid": "abc123", "headRefOid": "def456",
            "additions": 1, "deletions": 2, "changedFiles": 3, "reviewDecision": "CHANGES_REQUESTED"
        }"#;
        let meta: PrMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.base_ref_oid, "abc123");
        assert_eq!(meta.head_ref_oid, "def456");
        assert_eq!(meta.review_decision, "CHANGES_REQUESTED");

        // Older gh output without the field still deserializes.
        let json = json.replace(r#", "reviewDecision": "CHANGES_REQUESTED""#, "");
        let meta: PrMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(meta.review_decision, "");
    }

    #[test]
    fn deserializes_review_comments() {
        // Shaped like `gh api --paginate --slurp`: one array per page. The
        // REST payload is snake_case and carries fields we ignore.
        let json = r#"[[
            {
                "id": 100,
                "node_id": "x",
                "path": "src/main.rs",
                "line": 42,
                "side": "RIGHT",
                "start_line": 40,
                "start_side": "RIGHT",
                "body": "top-level comment",
                "user": {"login": "alice", "id": 1},
                "created_at": "2026-07-01T12:00:00Z",
                "in_reply_to_id": null
            },
            {
                "id": 101,
                "path": "src/main.rs",
                "line": 42,
                "side": "RIGHT",
                "start_line": null,
                "body": "a reply",
                "user": {"login": "bob"},
                "created_at": "2026-07-02T08:30:00Z",
                "in_reply_to_id": 100
            }
        ], [
            {
                "id": 102,
                "path": "old.rs",
                "line": null,
                "side": null,
                "start_line": null,
                "body": "outdated",
                "user": {"login": "carol"},
                "created_at": "2026-06-01T00:00:00Z"
            }
        ]]"#;
        let pages: Vec<Vec<ReviewComment>> = serde_json::from_str(json).unwrap();
        let comments: Vec<ReviewComment> = pages.into_iter().flatten().collect();
        assert_eq!(comments.len(), 3);
        let top = &comments[0];
        assert_eq!(top.id, 100);
        assert_eq!(top.path, "src/main.rs");
        assert_eq!(top.line, Some(42));
        assert_eq!(top.side.as_deref(), Some("RIGHT"));
        assert_eq!(top.start_line, Some(40));
        assert_eq!(top.user.login, "alice");
        assert_eq!(top.in_reply_to_id, None);
        let reply = &comments[1];
        assert_eq!(reply.in_reply_to_id, Some(100));
        assert_eq!(reply.start_line, None);
        // Outdated: null line/side, and a missing in_reply_to_id key.
        let outdated = &comments[2];
        assert_eq!(outdated.line, None);
        assert_eq!(outdated.side, None);
        assert_eq!(outdated.in_reply_to_id, None);
    }

    #[test]
    fn deserializes_pr_list_json() {
        let json = r#"[
            {
                "number": 3468,
                "title": "printer: add --field-name-terminator flag",
                "author": {"id": "x", "is_bot": false, "login": "alice", "name": "Alice"},
                "state": "OPEN",
                "isDraft": false,
                "headRefName": "field-name-terminator",
                "updatedAt": "2026-07-01T12:34:56Z"
            },
            {
                "number": 3470,
                "title": "wip: experiment",
                "author": {"login": "bob"},
                "state": "OPEN",
                "isDraft": true,
                "headRefName": "bob/wip",
                "updatedAt": "2026-06-30T08:00:00Z"
            }
        ]"#;
        let prs: Vec<PrSummary> = serde_json::from_str(json).unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 3468);
        assert_eq!(prs[0].author.login, "alice");
        assert_eq!(prs[0].state, "OPEN");
        assert!(!prs[0].is_draft);
        assert_eq!(prs[0].head_ref_name, "field-name-terminator");
        assert_eq!(prs[0].updated_at, "2026-07-01T12:34:56Z");
        assert!(prs[1].is_draft);
    }

    #[test]
    fn deserializes_project_field_list_json() {
        // Shaped like `gh project field-list --format json`.
        let json = r#"{
            "fields": [
                {
                    "id": "PVTF_1",
                    "name": "Status",
                    "type": "ProjectV2SingleSelectField",
                    "options": [
                        {"id": "opt_backlog", "name": "Backlog"},
                        {"id": "opt_todo", "name": "Todo"},
                        {"id": "opt_done", "name": "Done"}
                    ]
                },
                {
                    "id": "PVTF_2",
                    "name": "Due",
                    "type": "ProjectV2Field"
                }
            ]
        }"#;
        #[derive(serde::Deserialize)]
        struct Resp {
            fields: Vec<ProjectField>,
        }
        let resp: Resp = serde_json::from_str(json).unwrap();
        assert_eq!(resp.fields.len(), 2);
        assert_eq!(resp.fields[0].name, "Status");
        assert_eq!(resp.fields[0].options.len(), 3);
        assert_eq!(resp.fields[0].options[0].name, "Backlog");
        assert_eq!(resp.fields[1].name, "Due");
        assert!(resp.fields[1].options.is_empty());
    }

    #[test]
    fn deserializes_project_item_list_json() {
        // Shaped like `gh project item-list --format json`, mixing an
        // issue (kept), a draft issue and a PR (both dropped).
        let json = r#"{
            "items": [
                {
                    "id": "PVTI_1",
                    "content": {
                        "type": "Issue",
                        "number": 42,
                        "title": "printer: crashes on empty input",
                        "repository": "acme/widgets"
                    },
                    "status": "In Progress",
                    "due": "2026-09-20",
                    "priority": "High"
                },
                {
                    "id": "PVTI_2",
                    "content": {
                        "type": "DraftIssue",
                        "title": "sketch an idea"
                    }
                },
                {
                    "id": "PVTI_3",
                    "content": {
                        "type": "PullRequest",
                        "number": 7,
                        "title": "fix: printer crash",
                        "repository": "acme/widgets"
                    },
                    "status": "In Progress"
                }
            ],
            "totalCount": 3
        }"#;
        #[derive(serde::Deserialize)]
        struct Resp {
            items: Vec<ProjectItem>,
        }
        let resp: Resp = serde_json::from_str(json).unwrap();
        let issues: Vec<_> = resp
            .items
            .into_iter()
            .filter(|item| item.content.kind == "Issue")
            .collect();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].content.number, 42);
        assert_eq!(issues[0].status, "In Progress");
        assert_eq!(issues[0].due, "2026-09-20");
        assert_eq!(issues[0].priority, "High");
    }

    #[test]
    fn deserializes_issue_detail_graphql_json() {
        // Shaped like the `issue` field of `ISSUE_DETAIL_QUERY`'s response.
        let json = r#"{
            "title": "printer: crashes on empty input",
            "body": "Steps to reproduce...",
            "state": "OPEN",
            "url": "https://github.com/acme/widgets/issues/42",
            "createdAt": "2026-08-01T09:00:00Z",
            "assignees": {"nodes": [{"login": "alice"}]},
            "labels": {"nodes": [{"name": "bug", "color": "d73a4a"}]},
            "milestone": {"title": "v2.0", "dueOn": "2026-10-01T00:00:00Z"},
            "issueType": {"name": "Bug"},
            "subIssues": {"nodes": [{"number": 43, "title": "reproduce on CI", "state": "CLOSED"}]},
            "subIssuesSummary": {"total": 1, "completed": 1},
            "parent": {"number": 10, "title": "printer stability"},
            "closedByPullRequestsReferences": {
                "nodes": [{"number": 55, "state": "OPEN", "isDraft": false, "url": "https://github.com/acme/widgets/pull/55"}]
            },
            "comments": {
                "nodes": [{"author": {"login": "bob"}, "body": "looking into it", "createdAt": "2026-08-02T10:00:00Z"}]
            }
        }"#;
        let raw: RawIssueDetail = serde_json::from_str(json).unwrap();
        let detail: IssueDetail = raw.into();
        assert_eq!(detail.title, "printer: crashes on empty input");
        assert_eq!(detail.created_at, "2026-08-01T09:00:00Z");
        assert_eq!(detail.assignees, vec!["alice".to_string()]);
        assert_eq!(detail.labels[0].name, "bug");
        assert_eq!(detail.milestone.as_ref().unwrap().title, "v2.0");
        assert_eq!(
            detail.milestone.as_ref().unwrap().due_on.as_deref(),
            Some("2026-10-01T00:00:00Z")
        );
        assert_eq!(detail.issue_type.as_deref(), Some("Bug"));
        assert_eq!(detail.sub_issues.len(), 1);
        assert_eq!(detail.sub_issues[0].number, 43);
        assert_eq!(detail.sub_issues_summary.total, 1);
        assert_eq!(detail.sub_issues_summary.completed, 1);
        assert_eq!(detail.parent.as_ref().unwrap().number, 10);
        assert_eq!(detail.linked_prs[0].number, 55);
        assert!(!detail.linked_prs[0].is_draft);
        assert_eq!(detail.comments[0].author, "bob");
        assert_eq!(detail.comments[0].body, "looking into it");
    }

    #[test]
    fn deserializes_issue_detail_with_missing_optional_fields() {
        // No milestone, no issue type, no parent, no sub-issues: everything
        // that's `Option`/`Vec` should default rather than fail.
        let json = r#"{
            "title": "quick fix",
            "state": "OPEN",
            "url": "https://github.com/acme/widgets/issues/50",
            "createdAt": "2026-08-01T09:00:00Z",
            "assignees": {"nodes": []},
            "labels": {"nodes": []},
            "subIssues": {"nodes": []},
            "subIssuesSummary": {"total": 0, "completed": 0},
            "closedByPullRequestsReferences": {"nodes": []},
            "comments": {"nodes": []}
        }"#;
        let raw: RawIssueDetail = serde_json::from_str(json).unwrap();
        let detail: IssueDetail = raw.into();
        assert_eq!(detail.body, "");
        assert!(detail.milestone.is_none());
        assert!(detail.issue_type.is_none());
        assert!(detail.parent.is_none());
        assert!(detail.sub_issues.is_empty());
    }
}
