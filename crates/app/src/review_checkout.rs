use crate::cached_prs::{
    git_command, git_ok, is_linked_worktree_name, is_staged_clear_dir, sanitize_path_part,
};
use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static NEXT_REVIEW_ID: AtomicU64 = AtomicU64::new(0);

fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
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
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

#[derive(Clone)]
struct CachePaths {
    repos: PathBuf,
    worktrees: PathBuf,
    state: PathBuf,
}

impl CachePaths {
    fn new(root: &Path) -> Self {
        Self {
            repos: root.join("repos"),
            worktrees: root.join("worktrees"),
            state: root.join("checkout-state"),
        }
    }
}

fn cache_root() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".cache")
            .join("lgtm"),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CleanupStage {
    Active,
    Pending,
    StorageDestroyed,
}

impl CleanupStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
            Self::StorageDestroyed => "storage-destroyed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "active" => Some(Self::Active),
            "pending" => Some(Self::Pending),
            "storage-destroyed" => Some(Self::StorageDestroyed),
            _ => None,
        }
    }
}

pub(crate) struct CheckoutLease {
    _file: File,
}

impl CheckoutLease {
    fn acquire(path: &Path) -> io::Result<Self> {
        let RepoLock { _file } = RepoLock::acquire(path)?;
        Ok(Self { _file })
    }
}

struct RepoLock {
    _file: File,
}

impl RepoLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        lock_file(&file, false)?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn lock_file(file: &File, nonblocking: bool) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let mut flags = libc::LOCK_EX;
    if nonblocking {
        flags |= libc::LOCK_NB;
    }
    if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Default)]
struct JobCounter {
    state: Mutex<(bool, usize)>,
    wait: Condvar,
}

impl JobCounter {
    fn begin(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.0 {
            return false;
        }
        state.1 += 1;
        true
    }

    fn end(&self) {
        let mut state = self.state.lock().unwrap();
        state.1 -= 1;
        if state.1 == 0 {
            self.wait.notify_all();
        }
    }

    fn close(&self) {
        self.state.lock().unwrap().0 = true;
    }

    fn wait_zero(&self) {
        let mut state = self.state.lock().unwrap();
        while state.1 != 0 {
            state = self.wait.wait(state).unwrap();
        }
    }
}

pub(crate) struct SetupState {
    canceled: AtomicBool,
    jobs: JobCounter,
}

impl SetupState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            canceled: AtomicBool::new(false),
            jobs: JobCounter::default(),
        })
    }

    pub(crate) fn begin(self: &Arc<Self>) -> Option<SetupGuard> {
        if self.canceled.load(Ordering::Acquire) || !self.jobs.begin() {
            return None;
        }
        if self.canceled.load(Ordering::Acquire) {
            self.jobs.end();
            return None;
        }
        Some(SetupGuard {
            state: Arc::clone(self),
        })
    }

    pub(crate) fn cancel(&self) {
        self.canceled.store(true, Ordering::Release);
    }

    pub(crate) fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::Acquire)
    }

    pub(crate) fn wait_zero(&self) {
        self.jobs.wait_zero()
    }
}

pub(crate) struct SetupGuard {
    state: Arc<SetupState>,
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        self.state.jobs.end();
    }
}

#[derive(Default)]
pub(crate) struct CloseGate {
    jobs: JobCounter,
}

impl CloseGate {
    pub(crate) fn begin(self: &Arc<Self>) -> Option<CloseGuard> {
        self.jobs.begin().then_some(CloseGuard {
            gate: Arc::clone(self),
        })
    }

    pub(crate) fn close(&self) {
        self.jobs.close();
    }

    pub(crate) fn wait_zero(&self) {
        self.jobs.wait_zero()
    }
}

pub(crate) struct CloseGuard {
    gate: Arc<CloseGate>,
}

impl Drop for CloseGuard {
    fn drop(&mut self) {
        self.gate.jobs.end();
    }
}

struct CheckoutInner {
    paths: CachePaths,
    path: PathBuf,
    base_repo: PathBuf,
    marker: PathBuf,
    lease_path: PathBuf,
    lease: Arc<CheckoutLease>,
    loc: gh::PrLocator,
    head_oid: String,
}

#[derive(Clone)]
pub(crate) struct ReviewCheckout {
    inner: Arc<CheckoutInner>,
}

impl ReviewCheckout {
    pub(crate) fn create(loc: &gh::PrLocator, head_oid: &str) -> Result<Self> {
        let root = cache_root().context("HOME is not set")?;
        Self::create_at(&root, loc, head_oid)
    }

    pub(crate) fn create_at(root: &Path, loc: &gh::PrLocator, head_oid: &str) -> Result<Self> {
        validate_oid(head_oid)?;
        let paths = CachePaths::new(root);
        fs::create_dir_all(&paths.repos)?;
        fs::create_dir_all(&paths.worktrees)?;
        fs::create_dir_all(&paths.state)?;
        let repo_key = repo_key(loc);
        let base_repo = paths.repos.join(&repo_key);
        let _lock = RepoLock::acquire(&paths.repos.join(format!(".{repo_key}.lock")))?;
        ensure_base_repo(&base_repo, loc)?;
        let pull_ref = format!("refs/pull/{}/head", loc.number);
        git_ok(&base_repo, &["fetch", "--depth", "1", "origin", &pull_ref])?;
        let fetched = git_output(&base_repo, &["rev-parse", "FETCH_HEAD"])?;
        if !fetched.eq_ignore_ascii_case(head_oid) {
            bail!("PR head changed while fetching: expected {head_oid}, got {fetched}");
        }
        git_ok(
            &base_repo,
            &["cat-file", "-e", &format!("{head_oid}^{{commit}}")],
        )?;
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos() as u64);
        let (path, marker, lease_path, lease) = loop {
            let id = created_at.saturating_add(NEXT_REVIEW_ID.fetch_add(1, Ordering::Relaxed));
            let name = format!(
                "{}__{}__pr{}__{}__{}-{}",
                sanitize_path_part(&loc.owner),
                sanitize_path_part(&loc.repo),
                loc.number,
                &head_oid[..12],
                std::process::id(),
                id
            );
            let path = paths.worktrees.join(&name);
            let marker = paths.state.join(&name);
            let lease_path = paths.state.join(format!("{name}.lease"));
            if !path_is_free(&marker)? || !path_is_free(&path)? {
                continue;
            }
            let lease = match CheckoutLease::acquire(&lease_path) {
                Ok(lease) => Arc::new(lease),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                Err(err) => return Err(err.into()),
            };
            break (path, marker, lease_path, lease);
        };
        if let Err(err) = write_stage(&marker, CleanupStage::Active) {
            return Err(err);
        }
        if let Err(err) = git_ok(
            &base_repo,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().context("worktree path is not UTF-8")?,
                head_oid,
            ],
        ) {
            if let Err(state_err) = write_stage(&marker, CleanupStage::Pending) {
                eprintln!("lgtm: failed to retain checkout state after worktree add: {state_err}");
            }
            return Err(err);
        }
        Ok(Self {
            inner: Arc::new(CheckoutInner {
                paths,
                path,
                base_repo,
                marker,
                lease_path,
                lease,
                loc: loc.clone(),
                head_oid: head_oid.to_string(),
            }),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.inner.path
    }

    pub(crate) fn lease(&self) -> Arc<CheckoutLease> {
        Arc::clone(&self.inner.lease)
    }

    pub(crate) fn matches(&self, loc: &gh::PrLocator, head_oid: &str) -> bool {
        self.inner.loc.owner.eq_ignore_ascii_case(&loc.owner)
            && self.inner.loc.repo.eq_ignore_ascii_case(&loc.repo)
            && self.inner.loc.number == loc.number
            && self.inner.head_oid.eq_ignore_ascii_case(head_oid)
            && self.inner.path.starts_with(&self.inner.paths.worktrees)
    }

    pub(crate) fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn cleanup(self) -> Result<()> {
        let ReviewCheckout { inner } = self;
        let stage = read_stage(&inner.marker)?;
        if stage.is_none() {
            if matches!(
                fs::symlink_metadata(&inner.marker),
                Err(err) if err.kind() == io::ErrorKind::NotFound
            ) {
                remove_state_file(&inner.lease_path);
                return Ok(());
            }
            bail!("unknown checkout state: {}", inner.marker.display());
        }
        if stage != Some(CleanupStage::StorageDestroyed) {
            write_stage(&inner.marker, CleanupStage::Pending)?;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while Arc::strong_count(&inner) > 1 || Arc::strong_count(&inner.lease) > 1 {
            if std::time::Instant::now() >= deadline {
                bail!("checkout is still in use: {}", inner.path.display());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let path = inner.path.clone();
        let base_repo = inner.base_repo.clone();
        let marker = inner.marker.clone();
        let lease_path = inner.lease_path.clone();
        let paths = inner.paths.clone();
        let _lease = Arc::clone(&inner.lease);
        drop(inner);
        validate_owned_paths(&paths, &path, &base_repo, &marker, &lease_path)?;
        let _lock = RepoLock::acquire(&paths.repos.join(format!(
            ".{}.lock",
            base_repo.file_name().unwrap().to_string_lossy()
        )))?;
        if linked_admin(&base_repo, &path)?.is_none() {
            eprintln!(
                "lgtm: refusing to remove unregistered checkout {}",
                path.display()
            );
            if stage == Some(CleanupStage::StorageDestroyed) {
                remove_state_file(&marker);
                remove_state_file(&lease_path);
            }
            return Ok(());
        }
        if stage != Some(CleanupStage::StorageDestroyed) {
            destroy_linked_storage(&path, &base_repo)?;
            write_stage(&marker, CleanupStage::StorageDestroyed)?;
        }
        remove_worktree(&base_repo, &path)?;
        remove_state_file(&marker);
        remove_state_file(&lease_path);
        Ok(())
    }
}

fn validate_oid(oid: &str) -> Result<()> {
    if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("PR head oid is not a full SHA-1: {oid}");
    }
    Ok(())
}

fn path_is_free(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err.into()),
    }
}

fn ensure_base_repo(path: &Path, loc: &gh::PrLocator) -> Result<()> {
    let expected_origin = format!("https://github.com/{}.git", loc.repo_slug());
    if path.exists() {
        let origin = git_output(path, &["config", "--get", "remote.origin.url"])?;
        if !origin.eq_ignore_ascii_case(&expected_origin) {
            bail!("base repository origin does not match {}", expected_origin);
        }
        return Ok(());
    }
    let parent = path.parent().context("base repository has no parent")?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    let name = tmp
        .file_name()
        .and_then(|n| n.to_str())
        .context("temporary path is not UTF-8")?;
    let output = Command::new("git")
        .current_dir(parent)
        .args([
            "clone",
            "--no-checkout",
            "--filter=blob:none",
            &expected_origin,
            name,
        ])
        .output()?;
    if !output.status.success() {
        let _ = fs::remove_dir_all(&tmp);
        bail!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    fs::rename(tmp, path)?;
    Ok(())
}

fn remove_worktree(base_repo: &Path, path: &Path) -> Result<()> {
    if path.exists() {
        git_ok(
            base_repo,
            &[
                "worktree",
                "remove",
                "--force",
                path.to_str().context("worktree path is not UTF-8")?,
            ],
        )?;
    }
    git_ok(base_repo, &["worktree", "prune"])
}

fn linked_admin(base_repo: &Path, path: &Path) -> Result<Option<PathBuf>> {
    let expected = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let listing = git_output(base_repo, &["worktree", "list", "--porcelain"])?;
    if !listing.lines().any(|line| {
        line.strip_prefix("worktree ")
            .and_then(|value| PathBuf::from(value).canonicalize().ok())
            .is_some_and(|candidate| candidate == expected)
    }) {
        return Ok(None);
    }
    let admin = fs::canonicalize(git_admin_dir(path)?)?;
    let registry = fs::canonicalize(base_repo.join(".git"))?.join("worktrees");
    if !admin.starts_with(&registry) {
        return Ok(None);
    }
    let backlink = fs::canonicalize(fs::read_to_string(admin.join("gitdir"))?.trim())?;
    let expected = fs::canonicalize(path.join(".git"))?;
    Ok((backlink == expected).then_some(admin))
}

fn validate_owned_paths(
    paths: &CachePaths,
    path: &Path,
    base_repo: &Path,
    marker: &Path,
    lease: &Path,
) -> Result<()> {
    let root = fs::canonicalize(paths.repos.parent().context("repos has no parent")?)?;
    let worktrees = fs::canonicalize(&paths.worktrees)?;
    let repos = fs::canonicalize(&paths.repos)?;
    let state = fs::canonicalize(&paths.state)?;
    let path = fs::canonicalize(path)?;
    let base_repo = fs::canonicalize(base_repo)?;
    let base_git = fs::canonicalize(base_repo.join(".git"))?;
    let marker_parent = fs::canonicalize(marker.parent().context("marker has no parent")?)?;
    let lease_parent = fs::canonicalize(lease.parent().context("lease has no parent")?)?;
    if !worktrees.starts_with(&root)
        || !repos.starts_with(&root)
        || !state.starts_with(&root)
        || !path.starts_with(&worktrees)
        || !base_repo.starts_with(&repos)
        || !marker_parent.starts_with(&state)
        || !lease_parent.starts_with(&state)
        || !base_git.starts_with(&base_repo)
    {
        bail!("checkout paths are outside the owned cache");
    }
    Ok(())
}

fn destroy_linked_storage(path: &Path, base_repo: &Path) -> Result<()> {
    let leap = path.join("leap");
    if !proven_file(&leap.join("package.json"))? {
        return Ok(());
    }
    let leap = fs::canonicalize(leap)?;
    if !leap.starts_with(&fs::canonicalize(path)?) {
        bail!("Leap checkout escapes the review checkout");
    }
    let Some(admin) = linked_admin(base_repo, path)? else {
        bail!("Leap git admin directory is outside the checkout registry");
    };
    if fs::canonicalize(git_admin_dir(&leap)?)? != admin {
        bail!("Leap git admin directory does not belong to the checkout");
    }
    if !proven_file(&admin.join("leap-environment-id"))? {
        return Ok(());
    }
    run_storage_destroy(&leap)
}

fn run_storage_destroy(leap: &Path) -> Result<()> {
    let output = Command::new("vp")
        .current_dir(leap)
        .args(["run", "storage:destroy"])
        .output()
        .context("failed to run vp storage:destroy")?;
    if !output.status.success() {
        bail!(
            "vp run storage:destroy failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn proven_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.is_file() {
                Ok(true)
            } else {
                bail!("expected file is not a regular file: {}", path.display())
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn git_admin_dir(path: &Path) -> Result<PathBuf> {
    let output = git_command(path)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()?;
    if !output.status.success() {
        bail!(
            "git rev-parse failed in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let admin = String::from_utf8(output.stdout)?.trim().to_string();
    if admin.is_empty() {
        bail!(
            "git rev-parse returned an empty admin path for {}",
            path.display()
        );
    }
    Ok(PathBuf::from(admin))
}

fn write_stage(path: &Path, stage: CleanupStage) -> Result<()> {
    fs::write(path, format!("{}\n", stage.as_str()))?;
    Ok(())
}

fn remove_state_file(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => eprintln!(
            "lgtm: failed to remove cleanup state {}: {err}",
            path.display()
        ),
    }
}

fn read_stage(path: &Path) -> Result<Option<CleanupStage>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    Ok(CleanupStage::parse(fs::read_to_string(path)?.trim()))
}

fn lease_is_held(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)
    else {
        return true;
    };
    lock_file(&file, true).is_err()
}

fn parse_cached_name(name: &str) -> Option<gh::PrLocator> {
    let (owner_repo, rest) = name.rsplit_once("__pr")?;
    let (owner, repo) = owner_repo.split_once("__")?;
    let number = rest.split_once("__")?.0.parse().ok()?;
    (!owner.is_empty() && !repo.is_empty()).then(|| gh::PrLocator {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
    })
}

fn repo_key(loc: &gh::PrLocator) -> String {
    format!(
        "{}__{}",
        sanitize_path_part(&loc.owner).to_ascii_lowercase(),
        sanitize_path_part(&loc.repo).to_ascii_lowercase()
    )
}

pub(crate) fn cleanup_cached_dir(path: &Path) -> Result<()> {
    let root = cache_root().context("HOME is not set")?;
    cleanup_cached_dir_at(&root, path)
}

pub(crate) fn cleanup_cached_dir_at(root: &Path, path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("checkout path is not UTF-8")?;
    let Some(loc) = parse_cached_name(name) else {
        bail!("unrecognized checkout path");
    };
    if !is_linked_worktree_name(name) {
        bail!("legacy checkout requires startup migration");
    }
    let paths = CachePaths::new(root);
    let base = paths.repos.join(repo_key(&loc));
    let marker = paths.state.join(name);
    let lease = paths.state.join(format!("{name}.lease"));
    let checkout = detached_checkout(&paths, path, &base, &marker, &lease, loc)?;
    checkout.cleanup()
}

pub(crate) fn cleanup_all() {
    let Some(root) = cache_root() else {
        return;
    };
    cleanup_all_at(&root);
}

pub(crate) fn cleanup_all_at(root: &Path) {
    let paths = CachePaths::new(root);
    let _ = fs::create_dir_all(&paths.repos);
    let _ = fs::create_dir_all(&paths.state);
    let Ok(_migration_lock) = RepoLock::acquire(&paths.repos.join(".legacy-migration.lock")) else {
        return;
    };
    migrate_legacy(&paths);
    let Ok(entries) = fs::read_dir(&paths.state) else {
        return;
    };
    for entry in entries.flatten() {
        let marker = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".lease") || name.starts_with("legacy-") {
            continue;
        }
        if !is_linked_worktree_name(&name) {
            eprintln!("lgtm: refusing unknown checkout state {}", marker.display());
            continue;
        }
        let stage = match read_stage(&marker) {
            Ok(Some(stage)) => stage,
            Ok(None) => {
                eprintln!("lgtm: refusing unknown checkout state {}", marker.display());
                continue;
            }
            Err(err) => {
                eprintln!(
                    "lgtm: refusing unreadable checkout state {}: {err}",
                    marker.display()
                );
                continue;
            }
        };
        let Some(loc) = parse_cached_name(&name) else {
            continue;
        };
        let worktree = paths.worktrees.join(&name);
        let base = paths.repos.join(repo_key(&loc));
        if stage == CleanupStage::Active {
            if lease_is_held(&paths.state.join(format!("{name}.lease"))) {
                continue;
            }
            let Ok(age) = marker
                .metadata()
                .and_then(|metadata| metadata.modified())
                .and_then(|mtime| {
                    mtime
                        .elapsed()
                        .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
                })
            else {
                continue;
            };
            if age < Duration::from_secs(7 * 24 * 60 * 60) {
                continue;
            }
        }
        if let Ok(checkout) = detached_checkout(
            &paths,
            &worktree,
            &base,
            &marker,
            &paths.state.join(format!("{name}.lease")),
            loc,
        ) {
            if let Err(err) = checkout.cleanup() {
                eprintln!("lgtm: startup checkout cleanup pending: {err:#}");
            }
        }
    }
}

fn detached_checkout(
    paths: &CachePaths,
    path: &Path,
    base: &Path,
    marker: &Path,
    lease_path: &Path,
    loc: gh::PrLocator,
) -> Result<ReviewCheckout> {
    if !path.starts_with(&paths.worktrees)
        || !base.starts_with(&paths.repos)
        || !marker.starts_with(&paths.state)
    {
        bail!("cleanup path escapes owned cache");
    }
    let lease = Arc::new(CheckoutLease::acquire(lease_path)?);
    Ok(ReviewCheckout {
        inner: Arc::new(CheckoutInner {
            paths: CachePaths {
                repos: paths.repos.clone(),
                worktrees: paths.worktrees.clone(),
                state: paths.state.clone(),
            },
            path: path.to_path_buf(),
            base_repo: base.to_path_buf(),
            marker: marker.to_path_buf(),
            lease_path: lease_path.to_path_buf(),
            lease,
            loc,
            head_oid: String::new(),
        }),
    })
}

fn migrate_legacy(paths: &CachePaths) {
    let Ok(entries) = fs::read_dir(&paths.worktrees) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let legacy_name = if parse_cached_name(&name).is_some() && !is_linked_worktree_name(&name) {
            Some(name.clone())
        } else {
            staged_legacy_name(&name)
        };
        if legacy_name.is_none() || !path.is_dir() || !path.join(".git").is_dir() {
            continue;
        }
        let marker = paths.state.join(format!("legacy-{}", legacy_name.unwrap()));
        let stage = match read_stage(&marker) {
            Ok(Some(stage)) => stage,
            Ok(None) => {
                if let Err(err) = write_stage(&marker, CleanupStage::Pending) {
                    eprintln!(
                        "lgtm: legacy checkout state pending for {}: {err}",
                        path.display()
                    );
                    continue;
                }
                CleanupStage::Pending
            }
            Err(err) => {
                eprintln!(
                    "lgtm: refusing unreadable legacy checkout state {}: {err}",
                    marker.display()
                );
                continue;
            }
        };
        if stage != CleanupStage::StorageDestroyed {
            if let Err(err) = destroy_legacy_storage(paths, &path) {
                eprintln!(
                    "lgtm: legacy storage cleanup pending for {}: {err:#}",
                    path.display()
                );
                continue;
            }
            if let Err(err) = write_stage(&marker, CleanupStage::StorageDestroyed) {
                eprintln!(
                    "lgtm: legacy checkout checkpoint failed for {}: {err}",
                    path.display()
                );
                continue;
            }
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                remove_state_file(&marker);
            }
            Err(err) => eprintln!(
                "lgtm: legacy checkout removal pending for {}: {err}",
                path.display()
            ),
        }
    }
}

fn staged_legacy_name(name: &str) -> Option<String> {
    let (base, _) = name.rsplit_once(".tmp-clear-")?;
    let valid = is_staged_clear_dir(name)
        && parse_cached_name(base).is_some()
        && !is_linked_worktree_name(base);
    valid.then(|| name.to_string())
}

fn destroy_legacy_storage(paths: &CachePaths, path: &Path) -> Result<()> {
    let root = fs::canonicalize(&paths.worktrees)?;
    let path = fs::canonicalize(path)?;
    if !path.starts_with(&root) {
        bail!("legacy checkout is outside the owned cache");
    }
    let leap = path.join("leap");
    if !proven_file(&leap.join("package.json"))? {
        return Ok(());
    }
    let leap = fs::canonicalize(leap)?;
    if !leap.starts_with(&path) {
        bail!("legacy Leap checkout escapes the checkout");
    }
    let admin = fs::canonicalize(git_admin_dir(&path)?)?;
    if fs::canonicalize(git_admin_dir(&leap)?)? != admin || !admin.starts_with(&path.join(".git")) {
        bail!("legacy git admin directory is outside the checkout");
    }
    if !proven_file(&admin.join("leap-environment-id"))? {
        return Ok(());
    }
    run_storage_destroy(&leap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{mpsc, Mutex};

    static PATH_LOCK: Mutex<()> = Mutex::new(());

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct PathGuard(Option<OsString>);

    impl Drop for PathGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    fn git(dir: Option<&Path>, args: &[&str]) -> String {
        let mut command = Command::new("git");
        if let Some(dir) = dir {
            command.current_dir(dir);
        }
        let output = command.args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn fake_vp(root: &Path, status: u8) -> PathBuf {
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join("vp");
        fs::write(&path, format!("#!/bin/sh\nexit {status}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn close_join_waits_for_active_guard() {
        let gate = Arc::new(CloseGate::default());
        let guard = gate.begin().unwrap();
        gate.close();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker_gate = Arc::clone(&gate);
        let worker = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            worker_gate.wait_zero();
            done_tx.send(()).unwrap();
        });
        ready_rx.recv().unwrap();
        assert!(done_rx.try_recv().is_err());
        drop(guard);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn real_git_checkout_preserves_pending_storage_failure_then_retries() {
        let _path_lock = PATH_LOCK.lock().unwrap();
        let root = TempRoot(
            std::env::temp_dir().join(format!("lgtm-review-checkout-{}", std::process::id())),
        );
        let _ = fs::remove_dir_all(&root.0);
        fs::create_dir_all(&root.0).unwrap();
        let remote = root.0.join("remote.git");
        git(None, &["init", "--bare", remote.to_str().unwrap()]);
        let source = root.0.join("source");
        git(None, &["init", source.to_str().unwrap()]);
        git(Some(&source), &["config", "user.email", "test@example.com"]);
        git(Some(&source), &["config", "user.name", "Test"]);
        fs::write(source.join("README.md"), "review\n").unwrap();
        git(Some(&source), &["add", "README.md"]);
        git(Some(&source), &["commit", "-m", "initial"]);
        let head = git(Some(&source), &["rev-parse", "HEAD"]);
        git(
            Some(&source),
            &[
                "push",
                remote.to_str().unwrap(),
                &format!("HEAD:refs/pull/17/head"),
            ],
        );
        let base = root.0.join("repos/owner__repo.with.dot");
        fs::create_dir_all(base.parent().unwrap()).unwrap();
        git(
            None,
            &[
                "clone",
                "--no-checkout",
                remote.to_str().unwrap(),
                base.to_str().unwrap(),
            ],
        );
        let origin = "https://github.com/owner/repo.with.dot.git";
        git(Some(&base), &["config", "remote.origin.url", origin]);
        let rewrite = format!("url.{}.insteadOf", remote.display());
        git(Some(&base), &["config", &rewrite, origin]);
        let loc = gh::PrLocator {
            owner: "owner".to_string(),
            repo: "repo.with.dot".to_string(),
            number: 17,
        };
        let checkout = ReviewCheckout::create_at(&root.0, &loc, &head).unwrap();
        let checkout_path = checkout.root().to_path_buf();
        let marker = root
            .0
            .join("checkout-state")
            .join(checkout_path.file_name().unwrap());
        let admin = PathBuf::from(
            git_output(&checkout_path, &["rev-parse", "--absolute-git-dir"]).unwrap(),
        );
        fs::create_dir_all(checkout_path.join("leap")).unwrap();
        fs::write(checkout_path.join("leap/package.json"), "{}\n").unwrap();
        fs::write(admin.join("leap-environment-id"), "test\n").unwrap();

        cleanup_all_at(&root.0);
        assert!(checkout_path.exists());
        assert_eq!(read_stage(&marker).unwrap(), Some(CleanupStage::Active));
        let second = ReviewCheckout::create_at(&root.0, &loc, &head).unwrap();
        assert_ne!(checkout_path, second.root());
        let second_path = second.root().to_path_buf();
        git(Some(&source), &["checkout", "-q", "-b", "next"]);
        fs::write(source.join("next.txt"), "next\n").unwrap();
        git(Some(&source), &["add", "next.txt"]);
        git(Some(&source), &["commit", "-m", "next"]);
        let next_head = git(Some(&source), &["rev-parse", "HEAD"]);
        git(
            Some(&source),
            &[
                "push",
                remote.to_str().unwrap(),
                &format!("HEAD:refs/pull/17/head"),
            ],
        );
        let third = ReviewCheckout::create_at(&root.0, &loc, &next_head).unwrap();
        assert_ne!(second.root(), third.root());
        assert_eq!(
            git_output(third.root(), &["rev-parse", "HEAD"]).unwrap(),
            next_head
        );

        let bin = fake_vp(&root.0, 23);
        let old_path = std::env::var_os("PATH");
        let _path_guard = PathGuard(old_path.clone());
        let path = format!(
            "{}:{}",
            bin.parent().unwrap().display(),
            old_path.unwrap_or_default().to_string_lossy()
        );
        std::env::set_var("PATH", path);
        assert!(checkout.cleanup().is_err());
        assert!(checkout_path.exists());
        assert_eq!(read_stage(&marker).unwrap(), Some(CleanupStage::Pending));
        cleanup_all_at(&root.0);
        assert!(checkout_path.exists());

        fake_vp(&root.0, 0);
        cleanup_all_at(&root.0);
        assert!(!checkout_path.exists());
        assert!(!marker.exists());
        drop(second);
        let second_marker = root
            .0
            .join("checkout-state")
            .join(second_path.file_name().unwrap());
        fs::remove_file(&second_marker).unwrap();
        cleanup_cached_dir_at(&root.0, &second_path).unwrap();
        assert!(!second_marker.with_extension("lease").exists());
        third.cleanup().unwrap();

        let legacy_name = format!("owner__repo.with.dot__pr17__{next_head}");
        let legacy_path = root.0.join("worktrees").join(&legacy_name);
        git(
            None,
            &[
                "clone",
                "--no-checkout",
                remote.to_str().unwrap(),
                legacy_path.to_str().unwrap(),
            ],
        );
        fs::create_dir_all(legacy_path.join("leap")).unwrap();
        fs::write(legacy_path.join("leap/package.json"), "{}\n").unwrap();
        let legacy_admin = PathBuf::from(
            git_output(
                &legacy_path.join("leap"),
                &["rev-parse", "--absolute-git-dir"],
            )
            .unwrap(),
        );
        fs::write(legacy_admin.join("leap-environment-id"), "legacy\n").unwrap();
        fake_vp(&root.0, 23);
        cleanup_all_at(&root.0);
        assert!(legacy_path.exists());
        assert_eq!(
            read_stage(
                &root
                    .0
                    .join("checkout-state")
                    .join(format!("legacy-{legacy_name}"))
            )
            .unwrap(),
            Some(CleanupStage::Pending)
        );
        fake_vp(&root.0, 0);
        cleanup_all_at(&root.0);
        assert!(!legacy_path.exists());
    }
}
