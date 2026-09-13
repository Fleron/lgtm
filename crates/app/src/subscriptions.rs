use std::path::PathBuf;

pub(crate) fn parse_repo_slug(value: &str) -> Result<(String, String), &'static str> {
    let (owner, repo) = value.split_once('/').ok_or("expected owner/repo")?;
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return Err("expected owner/repo");
    }
    Ok((owner.to_string(), repo.to_string()))
}

#[derive(Clone)]
pub(crate) struct SubscribedRepo {
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) prs: Vec<gh::PrSummary>,
    pub(crate) refresh_error: Option<String>,
}

impl SubscribedRepo {
    pub(crate) fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

fn subscriptions_path() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".cache")
            .join("lgtm")
            .join("subscriptions.json"),
    )
}

pub(crate) fn parse_subscription_slugs(json: &str) -> Vec<(String, String)> {
    let Ok(slugs) = serde_json::from_str::<Vec<String>>(json) else {
        return Vec::new();
    };
    let mut parsed = Vec::new();
    for slug in slugs {
        let Ok((owner, repo)) = parse_repo_slug(&slug) else {
            continue;
        };
        if !parsed.iter().any(|(o, r)| o == &owner && r == &repo) {
            parsed.push((owner, repo));
        }
    }
    parsed
}

pub(crate) fn pr_key(owner: &str, repo: &str, number: u64) -> (String, String, u64) {
    (owner.to_lowercase(), repo.to_lowercase(), number)
}

pub(crate) fn load_subscribed_repos() -> Vec<SubscribedRepo> {
    let Some(path) = subscriptions_path() else {
        return Vec::new();
    };
    let Ok(json) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_subscription_slugs(&json)
        .into_iter()
        .map(|(owner, repo)| SubscribedRepo {
            owner,
            repo,
            prs: Vec::new(),
            refresh_error: None,
        })
        .collect()
}

pub(crate) fn save_subscribed_repos(repos: &[SubscribedRepo]) {
    let Some(path) = subscriptions_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let slugs: Vec<String> = repos.iter().map(SubscribedRepo::slug).collect();
    if let Ok(json) = serde_json::to_string_pretty(&slugs) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_subscription_slugs_parse_and_deduplicate() {
        assert_eq!(
            parse_repo_slug("owner/repo"),
            Ok(("owner".to_string(), "repo".to_string()))
        );
        assert!(parse_repo_slug("owner").is_err());
        assert!(parse_repo_slug("owner/repo/extra").is_err());

        assert_eq!(
            parse_subscription_slugs(
                r#"["owner/repo", "owner/repo", "other/project", "bad", "owner/repo/extra"]"#
            ),
            vec![
                ("owner".to_string(), "repo".to_string()),
                ("other".to_string(), "project".to_string()),
            ]
        );
        assert!(parse_subscription_slugs("not json").is_empty());
        assert_eq!(pr_key("Owner", "Repo", 42), pr_key("owner", "repo", 42));
    }

    #[test]
    fn repo_slug_parsing() {
        assert_eq!(
            parse_repo_slug("BurntSushi/ripgrep"),
            Ok(("BurntSushi".to_string(), "ripgrep".to_string()))
        );
        assert!(parse_repo_slug("ripgrep").is_err());
        assert!(parse_repo_slug("/ripgrep").is_err());
        assert!(parse_repo_slug("BurntSushi/").is_err());
        assert!(parse_repo_slug("a/b/c").is_err());
    }

}
