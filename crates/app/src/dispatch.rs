//! Dispatching a coding agent for a tracker issue: the `/`-completion pool
//! scanned from the `.claude` folders, and the Ghostty launcher. The pure
//! parts (scan, ranking, argv, checkout validation) are split from the spawn
//! so they can be tested without a terminal.

use gpui::{Context, Task, Window};
use gpui_component::input::{CompletionProvider, InputState};
use gpui_component::{Rope, RopeExt as _};
use std::path::{Path, PathBuf};

/// Shown in place of the error line when `dispatch_root` is unset.
pub(crate) const NO_ROOT_HINT: &str =
    "Set dispatch_root in ~/.cache/lgtm/tracker.json to the folder containing your checkouts";

/// Most completion items to offer at once.
const SKILL_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
        }
    }

    fn bin(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    pub(crate) fn toggled(self) -> Self {
        match self {
            Agent::Claude => Agent::Codex,
            Agent::Codex => Agent::Claude,
        }
    }
}

/// Where a dispatch runs: this machine, or a tmux session on an ssh host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Local,
    Remote(String),
}

impl Target {
    pub(crate) fn label(&self) -> &str {
        match self {
            Target::Local => "Local",
            Target::Remote(host) => host,
        }
    }
}

/// Host names from an `~/.ssh/config`, in file order, skipping patterns and
/// duplicates. `Include` directives are not followed.
pub(crate) fn ssh_hosts(config: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for line in config.lines() {
        let Some((key, rest)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        if !key.eq_ignore_ascii_case("Host") {
            continue;
        }
        for name in rest.split_whitespace() {
            if name.contains(['*', '?', '!']) || hosts.iter().any(|h| h == name) {
                continue;
            }
            hosts.push(name.to_string());
        }
    }
    hosts
}

/// Rejects anything that could carry shell metacharacters into the remote
/// script, which interpolates these two values unquoted in places.
fn shell_safe(value: &str, what: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("empty {what}"));
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Ok(());
    }
    Err(format!("{what} {value:?} is not a plain name"))
}

/// Arguments to `ssh` that start `agent` in a tmux window on `host`, in the
/// checkout for `repo`. A `root_override` is the only candidate tried, so a
/// wrong one fails with the exit-3 message instead of silently searching
/// elsewhere. The prompt travels base64-encoded and is decoded by the shell
/// tmux spawns, so no quoting of it survives into the script.
pub(crate) fn remote_args(
    host: &str,
    root_override: Option<&Path>,
    repo: &str,
    number: u64,
    agent: Agent,
    prompt: &str,
) -> Result<Vec<String>, String> {
    shell_safe(host, "host")?;
    shell_safe(repo, "repo")?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, prompt);
    let candidates = match root_override {
        Some(root) => {
            let root = root.display().to_string().replace('\'', r"'\''");
            format!("'{root}/{repo}'")
        }
        None => format!("\"$HOME/{repo}\" \"$HOME\"/*/{repo}"),
    };
    let agent = agent.bin();
    let script = format!(
        "d=\"\"; for c in {candidates}; \
         do [ -d \"$c\" ] && d=\"$c\" && break; done; \
         [ -z \"$d\" ] && {{ echo \"no checkout for {repo} under ~ on {host}\" >&2; exit 3; }}; \
         cmd=\"bash -lc '{agent} \\\"\\$(printf %s {b64} | base64 -d)\\\"'\"; \
         tmux has-session -t {agent} 2>/dev/null \
         && exec tmux new-window -t {agent}: -c \"$d\" -n '#{number}' \"$cmd\" \
         || exec tmux new-session -d -s {agent} -c \"$d\" -n '#{number}' \"$cmd\""
    );
    Ok(vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        host.to_string(),
        script,
    ])
}

/// Run `agent` against `prompt` in a tmux window on `host`. Returns the
/// remote stderr to show under the input when nothing started.
pub(crate) fn dispatch_remote(
    host: &str,
    root_override: Option<&Path>,
    repo: &str,
    number: u64,
    agent: Agent,
    prompt: &str,
) -> Result<(), String> {
    let args = remote_args(host, root_override, repo, number, agent, prompt)?;
    let out = std::process::Command::new("ssh")
        .args(args)
        .output()
        .map_err(|err| format!("could not run ssh: {err}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stderr.is_empty() {
        return Err(format!("ssh failed ({})", out.status));
    }
    Err(stderr)
}

/// `<root>/<repo>`, or why it can't be used as a working directory.
pub(crate) fn checkout_dir(root: &Path, repo: &str) -> Result<PathBuf, String> {
    let dir = root.join(repo);
    if dir.is_dir() {
        return Ok(dir);
    }
    Err(format!("{} is not a directory", dir.display()))
}

/// Arguments to `open` that put `agent` in a fresh Ghostty window at `dir`.
/// `-n` forces a new process rather than reusing a running Ghostty, and the
/// prompt rides as a separate argv element so `$0` receives it verbatim
/// without any shell quoting of our own.
pub(crate) fn open_args(dir: &Path, agent: Agent, prompt: &str) -> Vec<String> {
    vec![
        "-na".to_string(),
        "Ghostty.app".to_string(),
        "--args".to_string(),
        format!("--working-directory={}", dir.display()),
        "-e".to_string(),
        "/bin/zsh".to_string(),
        "-lic".to_string(),
        format!("{} \"$0\"", agent.bin()),
        prompt.to_string(),
    ]
}

/// Launch `agent` against `prompt` in `<root>/<repo>`. Returns the reason to
/// show under the input when nothing was started.
pub(crate) fn dispatch(root: &Path, repo: &str, agent: Agent, prompt: &str) -> Result<(), String> {
    let dir = checkout_dir(root, repo)?;
    let status = std::process::Command::new("open")
        .args(open_args(&dir, agent, prompt))
        .status()
        .map_err(|err| format!("could not run open: {err}"))?;
    if status.success() {
        return Ok(());
    }
    Err(format!("open failed ({status}), is Ghostty installed?"))
}

/// Every `.claude` folder worth scanning for skills and commands: the user's
/// own, then the checkout's if it exists.
fn claude_dirs(checkout: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".claude"));
    }
    if let Some(checkout) = checkout {
        dirs.push(checkout.join(".claude"));
    }
    dirs
}

/// Everything the card's `/` list offers: the user's own and the checkout's
/// `.claude` folders, plus every user-scoped plugin, whose entries are
/// namespaced `<plugin>:<name>`.
pub(crate) fn completion_names(checkout: Option<&Path>) -> Vec<String> {
    let plugins = std::env::var_os("HOME")
        .map(|home| {
            plugin_roots(
                &PathBuf::from(home)
                    .join(".claude")
                    .join("plugins")
                    .join("installed_plugins.json"),
            )
        })
        .unwrap_or_default();
    skill_names(&claude_dirs(checkout), &plugins)
}

/// Skill folder names (`skills/<name>/SKILL.md`) and command file stems
/// (`commands/**/*.md`) across `dirs`, then the same under each plugin root
/// prefixed with its plugin name, sorted and deduplicated.
fn skill_names(dirs: &[PathBuf], plugins: &[(String, PathBuf)]) -> Vec<String> {
    let mut names = Vec::new();
    for dir in dirs {
        collect_names(dir, None, &mut names);
    }
    for (plugin, root) in plugins {
        collect_names(root, Some(plugin), &mut names);
    }
    names.sort();
    names.dedup();
    names
}

/// The `skills/` and `commands/` pair under `root`, each name optionally
/// namespaced as `<prefix>:<name>`.
fn collect_names(root: &Path, prefix: Option<&str>, names: &mut Vec<String>) {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("skills")) {
        for entry in entries.flatten() {
            if !entry.path().join("SKILL.md").is_file() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                found.push(name.to_string());
            }
        }
    }
    collect_command_stems(&root.join("commands"), &mut found);
    match prefix {
        Some(prefix) => names.extend(found.iter().map(|name| format!("{prefix}:{name}"))),
        None => names.append(&mut found),
    }
}

#[derive(serde::Deserialize)]
struct InstalledPlugins {
    #[serde(default)]
    plugins: std::collections::BTreeMap<String, Vec<PluginInstall>>,
}

#[derive(serde::Deserialize)]
struct PluginInstall {
    scope: String,
    #[serde(rename = "installPath")]
    install_path: PathBuf,
}

/// User-scoped plugins from the installed-plugins manifest, as (plugin name,
/// install path). A missing or unparsable manifest contributes nothing.
fn plugin_roots(manifest: &Path) -> Vec<(String, PathBuf)> {
    let Ok(json) = std::fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<InstalledPlugins>(&json) else {
        return Vec::new();
    };
    parsed
        .plugins
        .into_iter()
        .flat_map(|(key, installs)| {
            // Manifest keys are "<plugin>@<marketplace>".
            let plugin = key.split('@').next().unwrap_or(&key).to_string();
            installs
                .into_iter()
                .filter(|install| install.scope == "user")
                .map(move |install| (plugin.clone(), install.install_path))
        })
        .collect()
}

fn collect_command_stems(dir: &Path, names: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_command_stems(&path, names);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            names.push(stem.to_string());
        }
    }
}

/// Skill and command names are alphanumeric plus hyphen and underscore; the
/// `/` must begin a word, so a path like `src/main` never triggers.
fn slash_prefix(text: &Rope, offset: usize) -> Option<(usize, String)> {
    crate::composer::sigil_prefix(text, offset, b'/', |c| {
        c.is_ascii_alphanumeric() || c == '-' || c == '_'
    })
}

/// Rank of `name` against `query` (matched case-insensitively), lower =
/// better; None = no match. Prefix beats substring.
fn skill_rank(name: &str, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let query = query.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    if name.starts_with(&query) {
        Some(0)
    } else if name.contains(&query) {
        Some(1)
    } else {
        None
    }
}

/// `/`-completion for the dispatch card, over the names collected once when
/// the card opened.
pub(crate) struct SkillProvider {
    pub(crate) names: Vec<String>,
}

impl CompletionProvider for SkillProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: lsp_types::CompletionContext,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        let empty = Task::ready(Ok(lsp_types::CompletionResponse::Array(vec![])));
        let Some((slash, prefix)) = slash_prefix(text, offset) else {
            return empty;
        };
        let range = lsp_types::Range {
            start: text.offset_to_position(slash),
            end: text.offset_to_position(offset),
        };
        let mut ranked: Vec<(u8, &String)> = self
            .names
            .iter()
            .filter_map(|name| skill_rank(name, &prefix).map(|rank| (rank, name)))
            .collect();
        // Stable sort keeps the alphabetical scan order within each rank.
        ranked.sort_by_key(|(rank, _)| *rank);
        let items = ranked
            .into_iter()
            .take(SKILL_LIMIT)
            .map(|(_, name)| lsp_types::CompletionItem {
                label: name.clone(),
                filter_text: Some(name.clone()),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range,
                    new_text: format!("/{name} "),
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
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("lgtm-dispatch-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn write(&self, rel: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn skill_names_collects_skills_and_nested_commands() {
        let tmp = TempDir::new("scan");
        tmp.write("home/.claude/skills/deep-review/SKILL.md");
        tmp.write("home/.claude/skills/no-manifest/README.md");
        tmp.write("home/.claude/commands/ship.md");
        tmp.write("home/.claude/commands/nested/deploy.md");
        tmp.write("home/.claude/commands/notes.txt");
        tmp.write("repo/.claude/skills/deep-review/SKILL.md");
        tmp.write("repo/.claude/skills/local-only/SKILL.md");

        let dirs = vec![tmp.0.join("home/.claude"), tmp.0.join("repo/.claude")];
        assert_eq!(
            skill_names(&dirs, &[]),
            vec!["deep-review", "deploy", "local-only", "ship"]
        );
    }

    #[test]
    fn skill_names_ignores_missing_dirs() {
        let tmp = TempDir::new("missing");
        assert!(skill_names(&[tmp.0.join("nope/.claude")], &[]).is_empty());
    }

    #[test]
    fn plugin_names_are_namespaced_and_skip_project_scope() {
        let tmp = TempDir::new("plugins");
        tmp.write("flow/skills/feature-planning/SKILL.md");
        tmp.write("flow/commands/ship-it.md");
        tmp.write("local/skills/not-yours/SKILL.md");
        let manifest = tmp.0.join("installed_plugins.json");
        let json = format!(
            r#"{{"version":2,"plugins":{{
                "development-flow@official":[{{"scope":"user","installPath":"{flow}","version":"1"}}],
                "local-only@official":[{{"scope":"project","installPath":"{local}"}}]
            }}}}"#,
            flow = tmp.0.join("flow").display(),
            local = tmp.0.join("local").display(),
        );
        std::fs::write(&manifest, json).unwrap();

        let roots = plugin_roots(&manifest);
        assert_eq!(roots, vec![("development-flow".to_string(), tmp.0.join("flow"))]);
        assert_eq!(
            skill_names(&[], &roots),
            vec!["development-flow:feature-planning", "development-flow:ship-it"]
        );
    }

    #[test]
    fn plugin_roots_tolerates_a_missing_or_broken_manifest() {
        let tmp = TempDir::new("manifest");
        assert!(plugin_roots(&tmp.0.join("absent.json")).is_empty());
        let broken = tmp.0.join("broken.json");
        std::fs::write(&broken, "{not json").unwrap();
        assert!(plugin_roots(&broken).is_empty());
    }

    #[test]
    fn slash_prefix_detects_tokens_at_word_boundaries() {
        let text = Rope::from("Issue #4: fix\n/dee");
        assert_eq!(slash_prefix(&text, 18), Some((14, "dee".to_string())));
        assert_eq!(slash_prefix(&text, 15), Some((14, String::new())));
        // Mid-word slashes (a path) never trigger.
        let path = Rope::from("src/main");
        assert_eq!(slash_prefix(&path, 8), None);
    }

    #[test]
    fn skill_rank_orders_prefix_before_substring() {
        assert_eq!(skill_rank("deep-review", "dee"), Some(0));
        assert_eq!(skill_rank("deep-review", "DEE"), Some(0));
        assert_eq!(skill_rank("code-review", "review"), Some(1));
        assert_eq!(skill_rank("code-review", ""), Some(0));
        assert_eq!(skill_rank("code-review", "zzz"), None);
    }

    #[test]
    fn open_args_passes_prompt_as_its_own_argument() {
        let args = open_args(Path::new("/checkouts/lgtm"), Agent::Codex, "do /it \"now\"");
        assert_eq!(
            args,
            vec![
                "-na",
                "Ghostty.app",
                "--args",
                "--working-directory=/checkouts/lgtm",
                "-e",
                "/bin/zsh",
                "-lic",
                "codex \"$0\"",
                "do /it \"now\"",
            ]
        );
        assert!(open_args(Path::new("/x"), Agent::Claude, "p")[7].starts_with("claude "));
    }

    #[test]
    fn checkout_dir_rejects_a_missing_checkout() {
        let tmp = TempDir::new("checkout");
        std::fs::create_dir_all(tmp.0.join("lgtm")).unwrap();
        assert_eq!(checkout_dir(&tmp.0, "lgtm"), Ok(tmp.0.join("lgtm")));
        assert!(checkout_dir(&tmp.0, "absent").is_err());
    }

    #[test]
    fn ssh_hosts_reads_host_lines_only() {
        let config = "\
Host devbox
  HostName 10.0.0.4
  User efleron

Host a b
\tHost indented
Host *
Host bad?name !nope
Host devbox
";
        assert_eq!(ssh_hosts(config), vec!["devbox", "a", "b", "indented"]);
        assert!(ssh_hosts("").is_empty());
    }

    #[test]
    fn remote_args_pin_the_exact_ssh_invocation() {
        let prompt = "Issue #7: \"quote\" $HOME `tick`\n/deep-review";
        let args = remote_args("devbox", None, "lgtm", 7, Agent::Claude, prompt).unwrap();
        assert_eq!(
            args,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "devbox",
                concat!(
                    r#"d=""; for c in "$HOME/lgtm" "$HOME"/*/lgtm; "#,
                    r#"do [ -d "$c" ] && d="$c" && break; done; "#,
                    r#"[ -z "$d" ] && { echo "no checkout for lgtm under ~ on devbox" >&2; exit 3; }; "#,
                    r#"cmd="bash -lc 'claude \"\$(printf %s "#,
                    "SXNzdWUgIzc6ICJxdW90ZSIgJEhPTUUgYHRpY2tgCi9kZWVwLXJldmlldw==",
                    r#" | base64 -d)\"'"; "#,
                    r#"tmux has-session -t claude 2>/dev/null "#,
                    r#"&& exec tmux new-window -t claude: -c "$d" -n '#7' "$cmd" "#,
                    r#"|| exec tmux new-session -d -s claude -c "$d" -n '#7' "$cmd""#,
                ),
            ]
        );
    }

    #[test]
    fn remote_args_use_a_configured_root_and_reject_odd_names() {
        let args =
            remote_args("box", Some(Path::new("/home/me/leap")), "lgtm", 1, Agent::Codex, "hi")
                .unwrap();
        // A configured root replaces the `~` search entirely.
        assert!(args[5].starts_with(r#"d=""; for c in '/home/me/leap/lgtm'; do"#));
        assert!(!args[5].contains("$HOME"));
        assert!(args[5].contains("bash -lc 'codex "));
        // A configured root is the user's own, but it still must not be able
        // to break out of the quoting.
        let odd = remote_args(
            "box",
            Some(Path::new("/it's $HOME/`x`/\"q\"")),
            "lgtm",
            1,
            Agent::Claude,
            "x",
        )
        .unwrap();
        assert!(odd[5].starts_with(
            "d=\"\"; for c in '/it'\\''s $HOME/`x`/\"q\"/lgtm'; do [ -d \"$c\" ]"
        ));
        assert!(remote_args("box", None, "a;rm -rf /", 1, Agent::Claude, "x").is_err());
        assert!(remote_args("a b", None, "lgtm", 1, Agent::Claude, "x").is_err());
        assert!(remote_args("box", None, "", 1, Agent::Claude, "x").is_err());
    }

    #[test]
    fn no_root_hint_names_the_config_key() {
        assert!(NO_ROOT_HINT.contains("dispatch_root"));
    }
}
