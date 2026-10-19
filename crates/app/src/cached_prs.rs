use crate::review_checkouts::ReviewCheckoutManager;
use std::path::PathBuf;

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

/// A PR whose LSP worktree(s) are cached on disk, surfaced in the sidebar so a
/// past review can be reopened or its cache cleaned up.
#[derive(Clone)]
pub(crate) struct CachedPr {
    pub(crate) loc: gh::PrLocator,
    /// Every cached worktree dir for this PR (one per reviewed head oid).
    pub(crate) dirs: Vec<PathBuf>,
}

#[derive(Default)]
pub(crate) struct CachedPrScan {
    pub(crate) cached_prs: Vec<CachedPr>,
}

pub(crate) fn scan_cached_prs_with(manager: &ReviewCheckoutManager) -> CachedPrScan {
    let mut grouped: Vec<(gh::PrLocator, Vec<PathBuf>)> = Vec::new();
    for record in manager.scan_cached() {
        let loc = gh::PrLocator {
            owner: record.owner,
            repo: record.repo,
            number: record.number,
        };
        let path = PathBuf::from(record.path);
        if let Some((_, dirs)) = grouped.iter_mut().find(|(existing, _)| {
            existing.owner == loc.owner
                && existing.repo == loc.repo
                && existing.number == loc.number
        }) {
            dirs.push(path);
        } else {
            grouped.push((loc, vec![path]));
        }
    }
    CachedPrScan {
        cached_prs: grouped
            .into_iter()
            .map(|(loc, dirs)| CachedPr { loc, dirs })
            .collect(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
