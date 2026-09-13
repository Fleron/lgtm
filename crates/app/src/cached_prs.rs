use anyhow::{anyhow, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Environment that makes every `git` we (or anything below us) run use the
/// token from `gh auth login` and never block on a prompt.
///
/// Per-command `-c` flags only reach the processes we spawn ourselves, but the
/// PR worktree is a `--filter=blob:none` partial clone: any tool that walks it —
/// bifrost, rust-analyzer, cargo — can trigger a lazy fetch of missing objects,
/// and that runs its own `git`. Those grandchildren are invisible to us, so the
/// config has to travel in the environment, which they inherit.
///
/// `GIT_CONFIG_COUNT`/`_KEY_n`/`_VALUE_n` is git's env spelling of `-c`. The
/// empty helper first clears any inherited one (macOS ships `osxkeychain` in
/// Xcode's system gitconfig) so it can't win or raise its own dialog.
///
/// `GIT_TERMINAL_PROMPT=0` is the load-bearing part: git asks for credentials on
/// `/dev/tty` directly, not stdin, so piping a child's stdio does *not* stop the
/// prompt — it just leaves it hanging on the launching terminal.
pub(crate) fn git_env() -> [(&'static str, &'static str); 6] {
    [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "credential.helper"),
        ("GIT_CONFIG_VALUE_0", ""),
        ("GIT_CONFIG_KEY_1", "credential.helper"),
        ("GIT_CONFIG_VALUE_1", "!gh auth git-credential"),
    ]
}

/// `~/.cache/lgtm/worktrees` — the parent of the per-PR LSP checkouts.
pub(crate) fn worktrees_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("lgtm")
            .join("worktrees"),
    )
}

/// A PR whose LSP worktree(s) are cached on disk, surfaced in the sidebar so a
/// past review can be reopened or its cache cleaned up.
#[derive(Clone)]
pub(crate) struct CachedPr {
    pub(crate) loc: gh::PrLocator,
    /// Every cached worktree dir for this PR (one per reviewed head oid).
    pub(crate) dirs: Vec<PathBuf>,
}

/// Scan the worktree cache for reviewable PRs, grouped by PR and most-recently
/// used first. Blocking (a `git` call per dir) — run off the UI thread.
pub(crate) fn scan_cached_prs() -> Vec<CachedPr> {
    let Some(root) = worktrees_root() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    // (locator, dirs, latest mtime) — one entry per distinct PR.
    let mut grouped: Vec<(gh::PrLocator, Vec<PathBuf>, std::time::SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Skip half-written clones from an in-progress materialize.
        if name.contains(".tmp-") {
            continue;
        }
        let Some(number) = cached_pr_number(&name) else {
            continue;
        };
        // The dir name sanitizes owner/repo lossily, so recover the real slug
        // from the clone's origin remote.
        let Some((owner, repo)) = worktree_remote(&path) else {
            continue;
        };
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match grouped
            .iter_mut()
            .find(|(l, ..)| l.owner == owner && l.repo == repo && l.number == number)
        {
            Some((_, dirs, latest)) => {
                dirs.push(path);
                *latest = (*latest).max(mtime);
            }
            None => grouped.push((
                gh::PrLocator {
                    owner,
                    repo,
                    number,
                },
                vec![path],
                mtime,
            )),
        }
    }
    grouped.sort_by(|a, b| b.2.cmp(&a.2));
    grouped
        .into_iter()
        .map(|(loc, dirs, _)| CachedPr { loc, dirs })
        .collect()
}

/// PR number from a `{owner}__{repo}__pr{N}__{oid}` worktree dir name.
pub(crate) fn cached_pr_number(dir_name: &str) -> Option<u64> {
    let after = dir_name.rsplit_once("__pr")?.1;
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Owner/repo from a cached clone's `origin` remote URL.
fn worktree_remote(dir: &Path) -> Option<(String, String)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    parse_github_owner_repo(url.trim())
}

/// Owner/repo from a GitHub remote URL (https, ssh, or scp-style).
pub(crate) fn parse_github_owner_repo(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("github.com/"))?;
    let (owner, repo) = rest.strip_suffix(".git").unwrap_or(rest).split_once('/')?;
    (!owner.is_empty() && !repo.is_empty()).then(|| (owner.to_string(), repo.to_string()))
}

pub(crate) fn sanitize_path_part(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `git` in `dir`. Auth and prompt suppression come from the process
/// environment (see `git_env`), which children inherit, so nothing extra is
/// needed here — and the same settings reach git processes we never spawn
/// ourselves.
pub(crate) fn git_command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd
}

pub(crate) fn git_ok(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = git_command(dir)
        .args(args)
        .output()
        .map_err(|err| anyhow!("failed to run git: {err}"))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn cached_pr_dir_names_parse() {
        assert_eq!(
            cached_pr_number("atuinsh__atuin__pr3592__5467566b7eb4"),
            Some(3592)
        );
        assert_eq!(
            cached_pr_number("oxidecomputer__propolis__pr966__cb6365959879"),
            Some(966)
        );
        // Repos/owners with underscores don't confuse the `__pr` split.
        assert_eq!(cached_pr_number("a_b__c_d__pr7__deadbeef"), Some(7));
        assert_eq!(cached_pr_number("no-number-here"), None);
    }

    #[test]
    fn github_remote_urls_parse_to_owner_repo() {
        let expect = Some(("atuinsh".to_string(), "atuin".to_string()));
        assert_eq!(
            parse_github_owner_repo("https://github.com/atuinsh/atuin.git"),
            expect
        );
        assert_eq!(
            parse_github_owner_repo("git@github.com:atuinsh/atuin.git"),
            expect
        );
        assert_eq!(
            parse_github_owner_repo("https://github.com/atuinsh/atuin"),
            expect
        );
        assert_eq!(parse_github_owner_repo("https://gitlab.com/a/b.git"), None);
    }

    #[test]
    fn git_env_routes_credentials_through_gh_and_never_prompts() {
        let env: HashMap<&str, &str> = git_env().into_iter().collect();
        // The prompt is the whole bug: git asks on /dev/tty, so piping a
        // child's stdio doesn't suppress it.
        assert_eq!(env.get("GIT_TERMINAL_PROMPT"), Some(&"0"));
        // GIT_CONFIG_COUNT must match the number of KEY/VALUE pairs or git
        // ignores the trailing ones (or errors).
        let count: usize = env.get("GIT_CONFIG_COUNT").unwrap().parse().unwrap();
        assert_eq!(count, 2);
        for ix in 0..count {
            assert!(
                env.contains_key(format!("GIT_CONFIG_KEY_{ix}").as_str())
                    && env.contains_key(format!("GIT_CONFIG_VALUE_{ix}").as_str()),
                "pair {ix} is incomplete"
            );
        }
        // Pair 0 clears any inherited helper (macOS ships osxkeychain via
        // Xcode's system gitconfig); pair 1 then installs gh's. Order matters:
        // reversed, the inherited helper would win.
        assert_eq!(env.get("GIT_CONFIG_KEY_0"), Some(&"credential.helper"));
        assert_eq!(env.get("GIT_CONFIG_VALUE_0"), Some(&""));
        assert_eq!(env.get("GIT_CONFIG_KEY_1"), Some(&"credential.helper"));
        assert_eq!(
            env.get("GIT_CONFIG_VALUE_1"),
            Some(&"!gh auth git-credential")
        );
    }

}
