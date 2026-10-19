use anyhow::{anyhow, bail, Context, Result};
use libc::{flock, LOCK_EX, LOCK_NB, LOCK_SH, LOCK_UN};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckoutSpec {
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) number: u64,
    pub(crate) head_oid: String,
}

impl CheckoutSpec {
    pub(crate) fn new(loc: &gh::PrLocator, head_oid: &str) -> Result<Self> {
        let owner = normalize_name(&loc.owner)?;
        let repo = normalize_name(&loc.repo)?;
        if !is_full_oid(head_oid) {
            bail!("PR head oid is not a full commit id: {head_oid}");
        }
        Ok(Self {
            owner,
            repo,
            number: loc.number,
            head_oid: head_oid.to_ascii_lowercase(),
        })
    }

    fn key(&self) -> String {
        format!(
            "{}__{}__pr{}__{}",
            self.owner,
            self.repo,
            self.number,
            &self.head_oid[..12]
        )
    }

    fn repo_key(&self) -> String {
        format!("{}__{}", self.owner, self.repo)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CheckoutRecord {
    pub(crate) key: String,
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) number: u64,
    pub(crate) head_oid: String,
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) git_admin_dir: Option<String>,
    #[serde(default)]
    pub(crate) created_secs: u64,
    #[serde(default)]
    pub(crate) last_used_secs: u64,
    #[serde(default)]
    pub(crate) pending_cleanup: bool,
    #[serde(default)]
    pub(crate) storage_teardown_complete: bool,
    #[serde(default)]
    pub(crate) storage_initialized_without_id: bool,
    #[serde(default)]
    pub(crate) materialized: bool,
    #[serde(default)]
    pub(crate) kind: RecordKind,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum RecordKind {
    #[default]
    Linked,
    Legacy,
}

#[derive(Default, Serialize, Deserialize)]
struct Metadata {
    #[serde(default)]
    records: Vec<CheckoutRecord>,
    #[serde(default)]
    migration_complete: bool,
}

struct ManagerInner {
    root: PathBuf,
    remote_override: Option<String>,
    startup_lock: Mutex<()>,
    startup_complete: AtomicBool,
}

#[derive(Clone)]
pub(crate) struct ReviewCheckoutManager {
    inner: Arc<ManagerInner>,
}

struct OperationState {
    closing: Arc<AtomicBool>,
    active: Mutex<usize>,
    idle: Condvar,
}

struct LeaseInner {
    manager: ReviewCheckoutManager,
    spec: CheckoutSpec,
    state: Arc<OperationState>,
    shared_lock: Mutex<Option<File>>,
    path: Mutex<Option<PathBuf>>,
    teardown_started: AtomicBool,
}

#[derive(Clone)]
pub(crate) struct ReviewCheckoutLease {
    inner: Arc<LeaseInner>,
}

pub(crate) struct CheckoutOperation {
    state: Arc<OperationState>,
}

impl Drop for CheckoutOperation {
    fn drop(&mut self) {
        let mut active = self.state.active.lock().unwrap_or_else(|e| e.into_inner());
        *active = active.saturating_sub(1);
        if *active == 0 {
            self.state.idle.notify_all();
        }
    }
}

impl ReviewCheckoutManager {
    pub(crate) fn new() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::at_root(home.join(".cache").join("lgtm"), None)
    }

    pub(crate) fn at_root(root: PathBuf, remote_override: Option<String>) -> Self {
        Self {
            inner: Arc::new(ManagerInner {
                root,
                remote_override,
                startup_lock: Mutex::new(()),
                startup_complete: AtomicBool::new(false),
            }),
        }
    }

    fn repos_root(&self) -> PathBuf {
        self.inner.root.join("repos")
    }

    fn worktrees_root(&self) -> PathBuf {
        self.inner.root.join("worktrees")
    }

    fn state_root(&self) -> PathBuf {
        self.inner.root.join("review-checkouts")
    }

    fn metadata_path(&self) -> PathBuf {
        self.state_root().join("metadata.json")
    }

    fn state_lock_path(&self) -> PathBuf {
        self.state_root().join("metadata.lock")
    }

    fn metadata_lock(&self) -> Result<File> {
        fs::create_dir_all(self.state_root())?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(self.state_lock_path())?;
        lock_file(&lock, LOCK_EX)?;
        Ok(lock)
    }

    fn metadata_lock_cancellable(&self, cancel: &AtomicBool) -> Result<File> {
        fs::create_dir_all(self.state_root())?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(self.state_lock_path())?;
        lock_file_cancellable(&lock, LOCK_EX, cancel)?;
        Ok(lock)
    }

    fn lease_path(&self, spec: &CheckoutSpec) -> PathBuf {
        self.state_root()
            .join("leases")
            .join(format!("{}.lock", spec.key()))
    }

    fn repo_lock_path(&self, spec: &CheckoutSpec) -> PathBuf {
        self.state_root()
            .join("repos")
            .join(format!("{}.lock", spec.repo_key()))
    }

    fn ensure_startup(&self) -> Result<()> {
        if self.inner.startup_complete.load(Ordering::Acquire) {
            return Ok(());
        }
        let _startup = self
            .inner
            .startup_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.inner.startup_complete.load(Ordering::Acquire) {
            return Ok(());
        }
        self.run_startup()?;
        self.inner.startup_complete.store(true, Ordering::Release);
        Ok(())
    }

    fn ensure_startup_cancellable(&self, cancel: &AtomicBool) -> Result<()> {
        check_acquire_cancelled(cancel)?;
        let _startup = loop {
            match self.inner.startup_lock.try_lock() {
                Ok(startup) => break startup,
                Err(TryLockError::Poisoned(startup)) => break startup.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    check_acquire_cancelled(cancel)?;
                    thread::sleep(Duration::from_millis(10));
                }
            }
        };
        if self.inner.startup_complete.load(Ordering::Acquire) {
            return Ok(());
        }
        check_acquire_cancelled(cancel)?;
        self.run_startup_cancellable(cancel)?;
        self.inner.startup_complete.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn startup(&self) -> Result<()> {
        self.ensure_startup()
    }

    pub(crate) fn acquire(
        &self,
        loc: &gh::PrLocator,
        head_oid: &str,
    ) -> Result<ReviewCheckoutLease> {
        self.ensure_startup()?;
        self.acquire_after_startup(loc, head_oid, None)
    }

    pub(crate) fn acquire_cancellable(
        &self,
        loc: &gh::PrLocator,
        head_oid: &str,
        cancel: &AtomicBool,
    ) -> Result<ReviewCheckoutLease> {
        self.ensure_startup_cancellable(cancel)?;
        self.acquire_after_startup(loc, head_oid, Some(cancel))
    }

    fn acquire_after_startup(
        &self,
        loc: &gh::PrLocator,
        head_oid: &str,
        cancel: Option<&AtomicBool>,
    ) -> Result<ReviewCheckoutLease> {
        let spec = CheckoutSpec::new(loc, head_oid)?;
        if let Some(cancel) = cancel {
            check_acquire_cancelled(cancel)?;
        }
        let path = self.lease_path(&spec);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;
        if let Some(cancel) = cancel {
            lock_file_cancellable(&file, LOCK_SH, cancel)?;
        } else {
            lock_file(&file, LOCK_SH)?;
        }
        let now = now_secs();
        let update = |record: &mut Option<CheckoutRecord>| {
            let record = record.get_or_insert_with(|| CheckoutRecord {
                key: spec.key(),
                owner: spec.owner.clone(),
                repo: spec.repo.clone(),
                number: spec.number,
                head_oid: spec.head_oid.clone(),
                path: self.worktrees_root().join(spec.key()).display().to_string(),
                git_admin_dir: None,
                created_secs: now,
                last_used_secs: now,
                pending_cleanup: false,
                storage_teardown_complete: false,
                storage_initialized_without_id: false,
                materialized: false,
                kind: RecordKind::Linked,
            });
            record.last_used_secs = now;
            record.pending_cleanup = false;
        };
        let update_metadata = |metadata: &mut Metadata| -> Result<()> {
            if metadata
                .records
                .iter()
                .any(|record| record.kind == RecordKind::Legacy && record.key == spec.key())
            {
                bail!(
                    "legacy review checkout still requires migration: {}",
                    spec.key()
                );
            }
            Self::update_record_in_metadata(metadata, &spec, update);
            Ok(())
        };
        if let Some(cancel) = cancel {
            self.with_metadata_cancellable(cancel, update_metadata)?;
        } else {
            self.with_metadata(update_metadata)?;
        }
        Ok(ReviewCheckoutLease {
            inner: Arc::new(LeaseInner {
                manager: self.clone(),
                spec,
                state: Arc::new(OperationState {
                    closing: Arc::new(AtomicBool::new(false)),
                    active: Mutex::new(0),
                    idle: Condvar::new(),
                }),
                shared_lock: Mutex::new(Some(file)),
                path: Mutex::new(None),
                teardown_started: AtomicBool::new(false),
            }),
        })
    }

    pub(crate) fn scan_cached(&self) -> Vec<CheckoutRecord> {
        let Ok(records) = self.read_records() else {
            return Vec::new();
        };
        records
            .records
            .into_iter()
            .filter(|record| {
                record.kind == RecordKind::Linked
                    && !record.pending_cleanup
                    && Path::new(&record.path).is_dir()
                    && self.checkout_is_idle(record)
            })
            .collect()
    }

    pub(crate) fn destroy_cached_path(&self, path: &Path) -> Result<bool> {
        self.ensure_startup()?;
        let selected = self
            .read_records()?
            .records
            .iter()
            .find(|record| Path::new(&record.path) == path)
            .cloned();
        let Some(selected) = selected else {
            return Ok(false);
        };
        let lease_path = self
            .state_root()
            .join("leases")
            .join(format!("{}.lock", selected.key));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lease_path)?;
        let spec = CheckoutSpec {
            owner: selected.owner.clone(),
            repo: selected.repo.clone(),
            number: selected.number,
            head_oid: selected.head_oid.clone(),
        };
        if try_lock_file(&file, LOCK_EX).is_err() {
            self.mark_pending(&spec)?;
            bail!("cached review checkout is still in use: {}", path.display());
        }
        let _metadata_lock = self.metadata_lock()?;
        let mut metadata = self.read_records_locked()?;
        let Some(record) = metadata
            .records
            .iter()
            .find(|record| record.key == selected.key && Path::new(&record.path) == path)
            .cloned()
        else {
            return Ok(false);
        };
        let lease = ReviewCheckoutLease {
            inner: Arc::new(LeaseInner {
                manager: self.clone(),
                spec,
                state: Arc::new(OperationState {
                    closing: Arc::new(AtomicBool::new(true)),
                    active: Mutex::new(0),
                    idle: Condvar::new(),
                }),
                shared_lock: Mutex::new(None),
                path: Mutex::new(Some(path.to_path_buf())),
                teardown_started: AtomicBool::new(true),
            }),
        };
        self.teardown_record_with_lock(&lease, file, record, &mut metadata)?;
        Ok(true)
    }

    fn checkout_is_idle(&self, record: &CheckoutRecord) -> bool {
        let Ok(file) = OpenOptions::new().read(true).write(true).create(true).open(
            self.state_root()
                .join("leases")
                .join(format!("{}.lock", record.key)),
        ) else {
            return false;
        };
        try_lock_file(&file, LOCK_EX).is_ok()
    }

    pub(crate) fn materialize(&self, lease: &ReviewCheckoutLease) -> Result<PathBuf> {
        let operation = lease.operation("materialize")?;
        let cancel = operation.cancel_token();
        let spec = &lease.inner.spec;
        let repo_lock = self.repo_lock_cancellable(spec, &cancel)?;
        let _metadata_lock = self.metadata_lock_cancellable(&cancel)?;
        let base = self.ensure_base(spec, &cancel)?;
        self.fetch_and_verify(&base, spec, &cancel)?;
        self.validate_base_layout(&base)?;
        let mut metadata = self.read_records_locked()?;
        let mut existing = None;
        for record in &metadata.records {
            if record.key != spec.key() || record.kind != RecordKind::Linked {
                continue;
            }
            if self.validate_record_path(record).is_err() {
                continue;
            }
            if !Path::new(&record.path).is_dir() {
                continue;
            }
            if registered_worktree_cancellable(
                &base,
                Path::new(&record.path),
                &spec.head_oid,
                &cancel,
            )? {
                existing = Some(record.clone());
                break;
            }
        }
        let path = if let Some(record) = existing {
            PathBuf::from(record.path)
        } else {
            let path = self.worktrees_root().join(spec.key());
            if path.exists() {
                bail!(
                    "registered review checkout path is not reusable: {}",
                    path.display()
                );
            }
            fs::create_dir_all(self.worktrees_root())?;
            run_git_cancellable(
                &base,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    path.to_str().context("checkout path is not UTF-8")?,
                    &spec.head_oid,
                ],
                &cancel,
            )?;
            path
        };
        if !registered_worktree_cancellable(&base, &path, &spec.head_oid, &cancel)? {
            bail!("git did not register review checkout {}", path.display());
        }
        let admin = git_output_cancellable(&path, &["rev-parse", "--absolute-git-dir"], &cancel)?;
        let admin = PathBuf::from(admin.trim());
        let now = now_secs();
        self.update_record_locked(&mut metadata, &spec, |record| {
            let record = record.get_or_insert_with(|| CheckoutRecord {
                key: spec.key(),
                owner: spec.owner.clone(),
                repo: spec.repo.clone(),
                number: spec.number,
                head_oid: spec.head_oid.clone(),
                path: path.display().to_string(),
                git_admin_dir: None,
                created_secs: now,
                last_used_secs: now,
                pending_cleanup: false,
                storage_teardown_complete: false,
                storage_initialized_without_id: false,
                materialized: true,
                kind: RecordKind::Linked,
            });
            record.path = path.display().to_string();
            record.git_admin_dir = Some(admin.display().to_string());
            record.materialized = true;
            record.last_used_secs = now;
            record.pending_cleanup = false;
            record.storage_teardown_complete = false;
            record.storage_initialized_without_id = false;
            record.kind = RecordKind::Linked;
        });
        self.write_records_locked(&metadata)?;
        *lease.inner.path.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.clone());
        drop(_metadata_lock);
        drop(repo_lock);
        Ok(path)
    }

    pub(crate) fn teardown(&self, lease: &ReviewCheckoutLease) -> Result<bool> {
        if lease.inner.teardown_started.swap(true, Ordering::AcqRel) {
            return Ok(false);
        }
        lease.mark_closing();
        let result = self.teardown_inner(lease);
        if !matches!(result, Ok(true)) {
            lease.inner.teardown_started.store(false, Ordering::Release);
        }
        result
    }

    fn teardown_inner(&self, lease: &ReviewCheckoutLease) -> Result<bool> {
        let mut active = lease
            .inner
            .state
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while *active != 0 {
            active = lease
                .inner
                .state
                .idle
                .wait(active)
                .unwrap_or_else(|e| e.into_inner());
        }
        drop(active);
        let mut shared = lease
            .inner
            .shared_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(file) = shared.as_ref() {
            unlock_file(file).context("releasing review checkout shared lock")?;
        }
        shared.take();
        drop(shared);
        let lease_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(self.lease_path(&lease.inner.spec))?;
        if try_lock_file(&lease_file, LOCK_EX).is_err() {
            lease.inner.teardown_started.store(false, Ordering::Release);
            return Ok(false);
        }
        let repo_lock = self.repo_lock(&lease.inner.spec)?;
        let record = self
            .read_records()?
            .records
            .into_iter()
            .find(|record| record.key == lease.inner.spec.key());
        let Some(record) = record else {
            drop(repo_lock);
            return Ok(true);
        };
        self.mark_pending(&lease.inner.spec)?;
        let path = self.validate_record_path(&record)?;
        let base = self.repos_root().join(lease.inner.spec.repo_key());
        if path.exists() {
            if !record.storage_teardown_complete {
                self.destroy_storage(&lease.inner.spec, &path, &record)?;
            }
            self.checkpoint(&lease.inner.spec, &record)?;
            match record.kind {
                RecordKind::Linked => {
                    if registered_path(&base, &path)? {
                        run_git(&base, &["worktree", "remove", "--force", &record.path])?;
                    } else {
                        fs::remove_dir_all(&path).with_context(|| {
                            format!("removing unregistered checkout {}", path.display())
                        })?;
                    }
                    self.prune_if_safe(&base)?;
                    if path.exists() || registered_path(&base, &path)? {
                        bail!("git retained review checkout {}", path.display());
                    }
                }
                RecordKind::Legacy => fs::remove_dir_all(&path)
                    .with_context(|| format!("removing legacy checkout {}", path.display()))?,
            }
        } else {
            let proven_safe = record.storage_teardown_complete
                || record.storage_initialized_without_id
                || (record.kind == RecordKind::Linked && !record.materialized);
            if !proven_safe {
                bail!(
                    "review checkout {} is missing without a storage teardown checkpoint",
                    path.display()
                );
            }
            if record.kind == RecordKind::Linked {
                self.prune_if_safe(&base)?;
                if registered_path(&base, &path)? {
                    bail!(
                        "git retained missing review checkout registration {}",
                        path.display()
                    );
                }
            }
        }
        self.remove_record(&lease.inner.spec)?;
        drop(repo_lock);
        Ok(true)
    }

    fn validate_record_path(&self, record: &CheckoutRecord) -> Result<PathBuf> {
        self.validate_managed_roots()?;
        let spec = record_spec(record)?;
        let path = PathBuf::from(&record.path);
        let root = self.worktrees_root();
        let root = root
            .canonicalize()
            .with_context(|| format!("resolving managed checkout root {}", root.display()))?;
        let parent = path
            .parent()
            .context("review checkout path has no parent")?
            .canonicalize()
            .with_context(|| format!("resolving checkout parent {}", path.display()))?;
        if parent != root || path.file_name().is_none() {
            bail!(
                "review checkout path is outside managed cache: {}",
                path.display()
            );
        }
        let name = path.file_name().unwrap().to_string_lossy();
        match record.kind {
            RecordKind::Linked if name != spec.key() => {
                bail!(
                    "linked review checkout path does not match its record: {}",
                    path.display()
                );
            }
            RecordKind::Legacy if !legacy_name_matches(&name, &spec) => {
                bail!(
                    "legacy review checkout path does not match its record: {}",
                    path.display()
                );
            }
            _ => {}
        }
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() {
                bail!("review checkout path is a symlink: {}", path.display());
            }
            let canonical = path.canonicalize()?;
            if canonical.parent() != Some(root.as_path()) {
                bail!(
                    "review checkout path is outside managed cache: {}",
                    path.display()
                );
            }
            match record.kind {
                RecordKind::Linked => {
                    self.validate_base_layout(&self.repos_root().join(spec.repo_key()))?;
                    let dot_git = path.join(".git");
                    let metadata = fs::symlink_metadata(&dot_git).with_context(|| {
                        format!("resolving linked checkout Git file {}", dot_git.display())
                    })?;
                    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                        bail!(
                            "linked checkout Git file is not managed: {}",
                            dot_git.display()
                        );
                    }
                    let admin = self.validate_admin_dir(&spec, &path, record)?;
                    self.validate_admin_backlink(&admin, &path)?;
                }
                RecordKind::Legacy => {
                    let dot_git = path.join(".git");
                    let metadata = fs::symlink_metadata(&dot_git).with_context(|| {
                        format!(
                            "resolving legacy checkout Git directory {}",
                            dot_git.display()
                        )
                    })?;
                    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                        bail!(
                            "legacy checkout Git directory is not managed: {}",
                            dot_git.display()
                        );
                    }
                    self.validate_admin_dir(&spec, &path, record)?;
                    let top = git_output(&path, &["rev-parse", "--show-toplevel"])?;
                    if PathBuf::from(top.trim()).canonicalize()? != path.canonicalize()? {
                        bail!("legacy checkout Git root does not own {}", path.display());
                    }
                    let remote = git_output(&path, &["config", "--get", "remote.origin.url"])?;
                    let Some((owner, repo)) =
                        crate::cached_prs::parse_github_owner_repo(remote.trim())
                    else {
                        bail!(
                            "legacy checkout origin is not a GitHub repository: {}",
                            path.display()
                        );
                    };
                    if !owner.eq_ignore_ascii_case(&spec.owner)
                        || !repo.eq_ignore_ascii_case(&spec.repo)
                    {
                        bail!(
                            "legacy checkout origin does not match its record: {}",
                            path.display()
                        );
                    }
                }
            }
        }
        Ok(path)
    }

    fn validate_admin_dir(
        &self,
        spec: &CheckoutSpec,
        path: &Path,
        record: &CheckoutRecord,
    ) -> Result<PathBuf> {
        let actual = PathBuf::from(
            git_output(path, &["rev-parse", "--absolute-git-dir"])
                .context("resolving checkout git admin directory")?
                .trim(),
        );
        if !actual.is_absolute() {
            bail!(
                "checkout git admin directory is not absolute: {}",
                actual.display()
            );
        }
        if let Some(saved) = record.git_admin_dir.as_deref() {
            let saved = PathBuf::from(saved)
                .canonicalize()
                .with_context(|| format!("resolving saved checkout git admin directory {saved}"))?;
            if saved != actual.canonicalize()? {
                bail!(
                    "checkout git admin directory changed for {}",
                    path.display()
                );
            }
        }
        let actual = actual.canonicalize()?;
        let allowed = match record.kind {
            RecordKind::Linked => {
                let base = self.repos_root().join(spec.repo_key());
                self.validate_base_layout(&base)?;
                base.join(".git").join("worktrees").canonicalize()?
            }
            RecordKind::Legacy => path.join(".git").canonicalize()?,
        };
        let owned = match record.kind {
            RecordKind::Linked => actual.parent() == Some(allowed.as_path()),
            RecordKind::Legacy => actual == allowed,
        };
        if !owned {
            bail!(
                "checkout git admin directory is outside managed checkout: {}",
                actual.display()
            );
        }
        Ok(actual)
    }

    fn validate_admin_backlink(&self, admin: &Path, path: &Path) -> Result<()> {
        let gitdir = admin.join("gitdir");
        let metadata = fs::symlink_metadata(&gitdir)
            .with_context(|| format!("resolving checkout admin backlink {}", gitdir.display()))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            bail!(
                "checkout admin backlink is not a regular file: {}",
                gitdir.display()
            );
        }
        let backlink = PathBuf::from(fs::read_to_string(&gitdir)?.trim());
        let backlink = if backlink.is_absolute() {
            backlink
        } else {
            admin.join(backlink)
        };
        if backlink.canonicalize()? != path.join(".git").canonicalize()? {
            bail!(
                "checkout git admin directory does not belong to {}",
                path.display()
            );
        }
        Ok(())
    }

    fn validate_storage_layout(&self, path: &Path) -> Result<()> {
        let leap = path.join("leap");
        let metadata = match fs::symlink_metadata(&leap) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            bail!("checkout leap directory is not managed: {}", leap.display());
        }
        let dot_git = leap.join(".git");
        match fs::symlink_metadata(&dot_git) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "checkout leap Git directory is a symlink: {}",
                    dot_git.display()
                )
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        Ok(())
    }

    fn prune_if_safe(&self, base: &Path) -> Result<()> {
        if !base.exists() {
            return Ok(());
        }
        let _lock = self.metadata_lock()?;
        let metadata = self.read_records_locked()?;
        self.prune_if_safe_locked(base, &metadata)
    }

    fn prune_if_safe_locked(&self, base: &Path, metadata: &Metadata) -> Result<()> {
        self.validate_managed_roots()?;
        if !base.exists() {
            if fs::symlink_metadata(base)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
            {
                bail!("managed base clone is a symlink: {}", base.display());
            }
            return Ok(());
        }
        let paths = managed_registered_paths(base)?;
        for path in paths {
            if same_path(&path, base) {
                continue;
            }
            if !metadata.records.iter().any(|record| {
                record.kind == RecordKind::Linked
                    && same_path(Path::new(&record.path), &path)
                    && self.validate_record_path(record).is_ok()
            }) {
                return Ok(());
            }
        }
        run_git(base, &["worktree", "prune"])
    }

    fn destroy_storage(
        &self,
        spec: &CheckoutSpec,
        path: &Path,
        record: &CheckoutRecord,
    ) -> Result<()> {
        let _lock = self.metadata_lock()?;
        let mut metadata = self.read_records_locked()?;
        self.destroy_storage_locked(spec, path, record, &mut metadata)
    }

    fn destroy_storage_locked(
        &self,
        spec: &CheckoutSpec,
        path: &Path,
        record: &CheckoutRecord,
        metadata: &mut Metadata,
    ) -> Result<()> {
        self.validate_storage_layout(path)?;
        let admin = self.validate_admin_dir(spec, path, record)?;
        let id_path = admin.join("leap-environment-id");
        let has_id = match fs::symlink_metadata(&id_path) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                true
            }
            Ok(_) => bail!(
                "checkout environment ID is not a regular file: {}",
                id_path.display()
            ),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => return Err(err.into()),
        };
        if !has_id {
            self.mark_no_id_in_metadata(metadata, record);
            return self.write_records_locked(metadata);
        }
        let package = path.join("leap").join("package.json");
        let package_exists = match fs::symlink_metadata(&package) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                true
            }
            Ok(_) => false,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => return Err(err.into()),
        };
        if !package_exists {
            bail!(
                "initialized checkout environment is missing {}",
                package.display()
            );
        }
        Self::update_record_in_metadata(metadata, spec, |current| {
            if let Some(current) = current {
                current.git_admin_dir = Some(admin.display().to_string());
            }
        });
        self.write_records_locked(metadata)?;
        let output = Command::new("vp")
            .args(["run", "storage:destroy"])
            .current_dir(path.join("leap"))
            .output()
            .map_err(|err| anyhow!("failed to run vp run storage:destroy: {err}"))?;
        if !output.status.success() {
            bail!(
                "vp run storage:destroy failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn mark_no_id_in_metadata(&self, metadata: &mut Metadata, record: &CheckoutRecord) {
        let spec = CheckoutSpec {
            owner: record.owner.clone(),
            repo: record.repo.clone(),
            number: record.number,
            head_oid: record.head_oid.clone(),
        };
        Self::update_record_in_metadata(metadata, &spec, |current| {
            if let Some(current) = current {
                current.storage_initialized_without_id = true;
            }
        });
    }

    fn checkpoint(&self, spec: &CheckoutSpec, record: &CheckoutRecord) -> Result<()> {
        let _lock = self.metadata_lock()?;
        let mut metadata = self.read_records_locked()?;
        self.checkpoint_in_metadata(spec, record, &mut metadata);
        self.write_records_locked(&metadata)
    }

    fn checkpoint_in_metadata(
        &self,
        spec: &CheckoutSpec,
        record: &CheckoutRecord,
        metadata: &mut Metadata,
    ) {
        Self::update_record_in_metadata(metadata, spec, |current| {
            if let Some(current) = current {
                if record.git_admin_dir.is_some() {
                    current.git_admin_dir = record.git_admin_dir.clone();
                }
                current.pending_cleanup = true;
                current.storage_teardown_complete = true;
            }
        });
    }

    fn mark_pending(&self, spec: &CheckoutSpec) -> Result<()> {
        self.with_metadata(|metadata| {
            Self::update_record_in_metadata(metadata, spec, |current| {
                if let Some(current) = current {
                    current.pending_cleanup = true;
                }
            });
            Ok(())
        })
    }

    fn run_startup(&self) -> Result<()> {
        let _metadata_lock = self.metadata_lock()?;
        self.run_startup_locked()
    }

    fn run_startup_cancellable(&self, cancel: &AtomicBool) -> Result<()> {
        let _metadata_lock = self.metadata_lock_cancellable(cancel)?;
        self.run_startup_locked()
    }

    fn run_startup_locked(&self) -> Result<()> {
        fs::create_dir_all(self.state_root().join("leases"))?;
        fs::create_dir_all(self.state_root().join("repos"))?;
        self.validate_managed_roots()?;
        self.reconcile_records_locked()?;
        self.migrate_legacy_locked()?;
        self.reconcile_records_locked()?;
        self.retain_old_locked()?;
        Ok(())
    }

    fn reconcile_records_locked(&self) -> Result<()> {
        let mut metadata = self.read_records_locked()?;
        let registry_records = metadata.records.clone();
        let mut changed = false;
        let mut retire = Vec::new();
        for record in &mut metadata.records {
            let path = match self.validate_record_path(record) {
                Ok(path) => path,
                Err(err) => {
                    record.pending_cleanup = true;
                    changed = true;
                    eprintln!("lgtm: review checkout reconciliation deferred: {err:#}");
                    continue;
                }
            };
            if path.exists() {
                if record.kind == RecordKind::Linked {
                    let spec = CheckoutSpec {
                        owner: record.owner.clone(),
                        repo: record.repo.clone(),
                        number: record.number,
                        head_oid: record.head_oid.clone(),
                    };
                    let Some(_repo_lock) = self.try_repo_lock(&spec)? else {
                        continue;
                    };
                    let base = self
                        .repos_root()
                        .join(format!("{}__{}", record.owner, record.repo));
                    match registered_path(&base, &path) {
                        Ok(true) => {
                            if let Ok(admin) =
                                git_output(&path, &["rev-parse", "--absolute-git-dir"])
                            {
                                record.git_admin_dir = Some(admin.trim().to_string());
                                changed = true;
                            }
                        }
                        Ok(false) => {
                            record.pending_cleanup = true;
                            changed = true;
                        }
                        Err(err) => {
                            record.pending_cleanup = true;
                            changed = true;
                            eprintln!("lgtm: review checkout registration reconciliation deferred: {err:#}");
                        }
                    }
                }
                continue;
            }
            let base = self
                .repos_root()
                .join(format!("{}__{}", record.owner, record.repo));
            let spec = CheckoutSpec {
                owner: record.owner.clone(),
                repo: record.repo.clone(),
                number: record.number,
                head_oid: record.head_oid.clone(),
            };
            let proven_safe = record.storage_teardown_complete
                || record.storage_initialized_without_id
                || (record.kind == RecordKind::Linked && !record.materialized);
            if proven_safe && record.kind == RecordKind::Linked {
                let Some(_repo_lock) = self.try_repo_lock(&spec)? else {
                    continue;
                };
                if base.exists() {
                    let metadata_snapshot = Metadata {
                        records: registry_records.clone(),
                        migration_complete: metadata.migration_complete,
                    };
                    if let Err(err) = self.prune_if_safe_locked(&base, &metadata_snapshot) {
                        record.pending_cleanup = true;
                        changed = true;
                        eprintln!("lgtm: review checkout reconciliation deferred: {err:#}");
                        continue;
                    }
                }
            }
            let registered = match registered_path(&base, &path) {
                Ok(registered) => registered,
                Err(err) => {
                    record.pending_cleanup = true;
                    changed = true;
                    eprintln!("lgtm: missing review checkout reconciliation deferred: {err:#}");
                    continue;
                }
            };
            if !registered && proven_safe {
                record.pending_cleanup = true;
                record.storage_teardown_complete = true;
                retire.push(record.key.clone());
                changed = true;
            } else if !registered || proven_safe {
                record.pending_cleanup = true;
                changed = true;
            }
        }
        if !retire.is_empty() {
            metadata
                .records
                .retain(|record| !retire.contains(&record.key));
        }
        let base_entries = fs::read_dir(self.repos_root()).with_context(|| {
            format!(
                "scanning cached repository root {}",
                self.repos_root().display()
            )
        })?;
        for base_entry in base_entries {
            let base_entry = base_entry.with_context(|| {
                format!(
                    "reading cached repository root {}",
                    self.repos_root().display()
                )
            })?;
            let base = base_entry.path();
            let base_metadata = match fs::symlink_metadata(&base) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if base_metadata.file_type().is_symlink() || !base_metadata.file_type().is_dir() {
                continue;
            }
            let Some(repo_key) = base.file_name() else {
                continue;
            };
            let repo_lock_path = self
                .state_root()
                .join("repos")
                .join(format!("{}.lock", repo_key.to_string_lossy()));
            let repo_lock = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(repo_lock_path)?;
            if try_lock_file(&repo_lock, LOCK_EX).is_err() {
                continue;
            }
            let paths = match registered_paths(&base) {
                Ok(paths) => paths,
                Err(err) => {
                    eprintln!(
                        "lgtm: review checkout registration scan deferred for {}: {err:#}",
                        base.display()
                    );
                    continue;
                }
            };
            for path in paths {
                if !path.starts_with(self.worktrees_root()) || !path.is_dir() {
                    continue;
                }
                if metadata
                    .records
                    .iter()
                    .any(|record| Path::new(&record.path) == path)
                {
                    continue;
                }
                if let Some(record) = infer_record(&base, &path) {
                    if self.validate_record_path(&record).is_err() {
                        continue;
                    }
                    metadata.records.push(record);
                    changed = true;
                }
            }
        }
        if changed {
            self.write_records_locked(&metadata)?;
        }
        Ok(())
    }

    fn migrate_legacy_locked(&self) -> Result<()> {
        let mut metadata = self.read_records_locked()?;
        if metadata.migration_complete {
            return Ok(());
        }
        let root = self.worktrees_root();
        let root_canonical = match root.canonicalize() {
            Ok(root) => root,
            Err(err) => {
                eprintln!(
                    "lgtm: legacy checkout migration root scan deferred for {}: {err:#}",
                    root.display()
                );
                metadata.migration_complete = false;
                self.write_records_locked(&metadata)?;
                return Ok(());
            }
        };
        let mut failed = false;
        let entries = match fs::read_dir(&root) {
            Ok(entries) => Some(entries),
            Err(err) => {
                failed = true;
                eprintln!(
                    "lgtm: legacy checkout migration scan deferred for {}: {err:#}",
                    root.display()
                );
                None
            }
        };
        if let Some(entries) = entries {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        failed = true;
                        eprintln!("lgtm: legacy checkout migration entry scan deferred: {err:#}");
                        continue;
                    }
                };
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(err) => {
                        failed = true;
                        eprintln!(
                            "lgtm: legacy checkout migration metadata scan deferred for {}: {err:#}",
                            path.display()
                        );
                        continue;
                    }
                };
                let path_canonical = match path.canonicalize() {
                    Ok(path) => path,
                    Err(err) => {
                        failed = true;
                        eprintln!(
                            "lgtm: legacy checkout migration path scan deferred for {}: {err:#}",
                            path.display()
                        );
                        continue;
                    }
                };
                if !file_type.is_dir()
                    || file_type.is_symlink()
                    || !path_canonical.starts_with(&root_canonical)
                {
                    continue;
                }
                if path.join(".git").is_file() {
                    continue;
                }
                let Some((owner, repo, number, head_oid)) = (match legacy_identity(&path) {
                    Ok(identity) => identity,
                    Err(err) => {
                        failed = true;
                        eprintln!(
                            "lgtm: legacy checkout identity scan deferred for {}: {err:#}",
                            path.display()
                        );
                        continue;
                    }
                }) else {
                    continue;
                };
                let spec = CheckoutSpec {
                    owner,
                    repo,
                    number,
                    head_oid,
                };
                let record = metadata
                    .records
                    .iter()
                    .find(|existing| existing.path == path.display().to_string())
                    .cloned()
                    .unwrap_or_else(|| CheckoutRecord {
                        key: spec.key(),
                        owner: spec.owner.clone(),
                        repo: spec.repo.clone(),
                        number: spec.number,
                        head_oid: spec.head_oid.clone(),
                        path: path.display().to_string(),
                        git_admin_dir: git_output(&path, &["rev-parse", "--absolute-git-dir"])
                            .ok()
                            .map(|s| s.trim().to_string()),
                        created_secs: now_secs(),
                        last_used_secs: now_secs(),
                        pending_cleanup: true,
                        storage_teardown_complete: false,
                        storage_initialized_without_id: false,
                        materialized: true,
                        kind: RecordKind::Legacy,
                    });
                metadata
                    .records
                    .retain(|existing| existing.path != record.path);
                metadata.records.push(record.clone());
                self.write_records_locked(&metadata)?;
                let result = (|| -> Result<()> {
                    let Some(_repo_lock) = self.try_repo_lock(&spec)? else {
                        bail!("repository is busy: {}", spec.repo_key());
                    };
                    let path = self.validate_record_path(&record)?;
                    if !record.storage_teardown_complete {
                        self.destroy_storage_locked(&spec, &path, &record, &mut metadata)?;
                    }
                    self.checkpoint_in_metadata(&spec, &record, &mut metadata);
                    self.write_records_locked(&metadata)?;
                    fs::remove_dir_all(&path)?;
                    Ok(())
                })();
                if let Err(err) = result {
                    eprintln!(
                        "lgtm: legacy checkout migration deferred for {}: {err:#}",
                        path.display()
                    );
                    failed = true;
                } else {
                    metadata
                        .records
                        .retain(|existing| existing.path != record.path);
                }
            }
        }
        metadata.migration_complete = !failed;
        self.write_records_locked(&metadata)
    }

    fn retain_old_locked(&self) -> Result<()> {
        let cutoff = now_secs().saturating_sub(RETENTION_SECS);
        let mut metadata = self.read_records_locked()?;
        let records = metadata.records.clone();
        for record in records {
            if !record.pending_cleanup && record.last_used_secs >= cutoff {
                continue;
            }
            let lease_path = self
                .state_root()
                .join("leases")
                .join(format!("{}.lock", record.key));
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(lease_path)?;
            if try_lock_file(&file, LOCK_EX).is_err() {
                continue;
            }
            let spec = CheckoutSpec {
                owner: record.owner.clone(),
                repo: record.repo.clone(),
                number: record.number,
                head_oid: record.head_oid.clone(),
            };
            let lease = ReviewCheckoutLease {
                inner: Arc::new(LeaseInner {
                    manager: self.clone(),
                    spec,
                    state: Arc::new(OperationState {
                        closing: Arc::new(AtomicBool::new(true)),
                        active: Mutex::new(0),
                        idle: Condvar::new(),
                    }),
                    shared_lock: Mutex::new(None),
                    path: Mutex::new(Some(PathBuf::from(&record.path))),
                    teardown_started: AtomicBool::new(true),
                }),
            };
            if let Err(err) = self.teardown_record_with_lock(&lease, file, record, &mut metadata) {
                eprintln!("lgtm: retained checkout cleanup deferred: {err:#}");
            }
        }
        self.write_records_locked(&metadata)
    }

    fn teardown_record_with_lock(
        &self,
        lease: &ReviewCheckoutLease,
        lease_file: File,
        record: CheckoutRecord,
        metadata: &mut Metadata,
    ) -> Result<()> {
        Self::update_record_in_metadata(metadata, &lease.inner.spec, |current| {
            if let Some(current) = current {
                current.pending_cleanup = true;
            }
        });
        self.write_records_locked(metadata)?;
        let Some(repo_lock) = self.try_repo_lock(&lease.inner.spec)? else {
            bail!("repository is busy: {}", lease.inner.spec.repo_key());
        };
        let path = self.validate_record_path(&record)?;
        let base = self.repos_root().join(lease.inner.spec.repo_key());
        if path.exists() {
            if !record.storage_teardown_complete {
                self.destroy_storage_locked(&lease.inner.spec, &path, &record, metadata)?;
            }
            self.checkpoint_in_metadata(&lease.inner.spec, &record, metadata);
            self.write_records_locked(metadata)?;
            match record.kind {
                RecordKind::Linked => {
                    if registered_path(&base, &path)? {
                        run_git(&base, &["worktree", "remove", "--force", &record.path])?;
                    } else {
                        fs::remove_dir_all(&path).with_context(|| {
                            format!("removing unregistered checkout {}", path.display())
                        })?;
                    }
                    self.prune_if_safe_locked(&base, metadata)?;
                }
                RecordKind::Legacy => fs::remove_dir_all(&path)?,
            }
        } else {
            let proven_safe = record.storage_teardown_complete
                || record.storage_initialized_without_id
                || (record.kind == RecordKind::Linked && !record.materialized);
            if !proven_safe {
                bail!("missing checkout {} has no teardown proof", path.display());
            }
            if record.kind == RecordKind::Linked {
                self.prune_if_safe_locked(&base, metadata)?;
                if registered_path(&base, &path)? {
                    bail!(
                        "git retained missing review checkout registration {}",
                        path.display()
                    );
                }
            }
        }
        self.remove_record_locked(metadata, &lease.inner.spec);
        self.write_records_locked(metadata)?;
        drop(repo_lock);
        unlock_file(&lease_file)?;
        Ok(())
    }

    fn ensure_base(&self, spec: &CheckoutSpec, cancel: &AtomicBool) -> Result<PathBuf> {
        check_cancelled(cancel)?;
        self.validate_managed_roots()?;
        let base = self.repos_root().join(spec.repo_key());
        match fs::symlink_metadata(&base) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("cached base clone is a symlink: {}", base.display())
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        if base.exists() {
            self.validate_base_layout(&base)?;
            let remote =
                git_output_cancellable(&base, &["config", "--get", "remote.origin.url"], cancel)?;
            if !self.remote_matches(spec, remote.trim()) {
                bail!("cached base clone has the wrong origin: {}", base.display());
            }
            return Ok(base);
        }
        let tmp = self
            .repos_root()
            .join(format!("{}.tmp-{}", spec.repo_key(), std::process::id()));
        if tmp.exists() {
            bail!("partial base clone already exists: {}", tmp.display());
        }
        let url = self
            .inner
            .remote_override
            .clone()
            .unwrap_or_else(|| format!("https://github.com/{}/{}.git", spec.owner, spec.repo));
        let clone_result = run_command_cancellable(
            &mut Command::new("git")
                .args(["clone", "--no-checkout", "--filter=blob:none", &url])
                .arg(&tmp),
            cancel,
        );
        if let Err(err) = clone_result {
            let _ = fs::remove_dir_all(&tmp);
            return Err(err);
        }
        if let Err(err) = check_cancelled(cancel) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(err);
        }
        if let Err(err) = self.validate_base_layout(&tmp) {
            let _ = fs::remove_dir_all(&tmp);
            return Err(err);
        }
        if let Err(err) = fs::rename(&tmp, &base) {
            if base.is_dir() {
                return Ok(base);
            }
            let _ = fs::remove_dir_all(&tmp);
            return Err(err.into());
        }
        Ok(base)
    }

    fn validate_base_layout(&self, base: &Path) -> Result<()> {
        let base_metadata = fs::symlink_metadata(base)
            .with_context(|| format!("resolving cached base clone {}", base.display()))?;
        if base_metadata.file_type().is_symlink() {
            bail!("cached base clone is a symlink: {}", base.display());
        }
        let git_dir = base.join(".git");
        let git_metadata = fs::symlink_metadata(&git_dir).with_context(|| {
            format!("resolving cached base Git directory {}", git_dir.display())
        })?;
        if git_metadata.file_type().is_symlink() {
            bail!(
                "cached base Git directory is a symlink: {}",
                git_dir.display()
            );
        }
        if !git_metadata.file_type().is_dir() {
            bail!(
                "cached base Git directory is not managed: {}",
                git_dir.display()
            );
        }
        let worktrees = git_dir.join("worktrees");
        match fs::symlink_metadata(&worktrees) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "cached base worktree admin root is a symlink: {}",
                    worktrees.display()
                )
            }
            Ok(metadata) if !metadata.file_type().is_dir() => {
                bail!(
                    "cached base worktree admin root is not a directory: {}",
                    worktrees.display()
                )
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        Ok(())
    }

    fn validate_managed_roots(&self) -> Result<()> {
        let roots = [self.repos_root(), self.worktrees_root()];
        for root in &roots {
            match fs::symlink_metadata(root) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("managed checkout root is a symlink: {}", root.display())
                }
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        for root in &roots {
            fs::create_dir_all(root)?;
        }
        let cache_root = self.inner.root.canonicalize().with_context(|| {
            format!(
                "resolving configured cache root {}",
                self.inner.root.display()
            )
        })?;
        for root in &roots {
            let metadata = fs::symlink_metadata(root)?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                bail!(
                    "managed checkout root is not a directory: {}",
                    root.display()
                );
            }
            let canonical = root.canonicalize()?;
            if !canonical.starts_with(&cache_root) {
                bail!(
                    "managed checkout root is outside configured cache: {}",
                    root.display()
                );
            }
        }
        Ok(())
    }

    fn remote_matches(&self, spec: &CheckoutSpec, actual: &str) -> bool {
        if let Some(expected) = self.inner.remote_override.as_deref() {
            return same_remote(expected, actual);
        }
        crate::cached_prs::parse_github_owner_repo(actual).is_some_and(|(owner, repo)| {
            owner.eq_ignore_ascii_case(&spec.owner) && repo.eq_ignore_ascii_case(&spec.repo)
        })
    }

    fn fetch_and_verify(
        &self,
        base: &Path,
        spec: &CheckoutSpec,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let pull_ref = format!("refs/pull/{}/head", spec.number);
        let local_ref = format!("refs/lgtm/pull/{}", spec.number);
        run_git_cancellable(
            base,
            &[
                "fetch",
                "--filter=blob:none",
                "origin",
                &format!("+{pull_ref}:{local_ref}"),
            ],
            cancel,
        )?;
        let fetched = git_output_cancellable(
            base,
            &["rev-parse", &format!("{local_ref}^{{commit}}")],
            cancel,
        )?;
        if fetched.trim() != spec.head_oid {
            bail!(
                "fetched PR head {} instead of requested {}",
                fetched.trim(),
                spec.head_oid
            );
        }
        let full = git_output_cancellable(
            base,
            &["cat-file", "-e", &format!("{}^{{commit}}", spec.head_oid)],
            cancel,
        );
        if full.is_err() {
            bail!("requested PR head is not a complete commit in base clone");
        }
        Ok(())
    }

    fn repo_lock(&self, spec: &CheckoutSpec) -> Result<File> {
        let path = self.repo_lock_path(spec);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        lock_file(&file, LOCK_EX)?;
        Ok(file)
    }

    fn repo_lock_cancellable(&self, spec: &CheckoutSpec, cancel: &AtomicBool) -> Result<File> {
        let path = self.repo_lock_path(spec);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        lock_file_cancellable(&file, LOCK_EX, cancel)?;
        Ok(file)
    }

    fn try_repo_lock(&self, spec: &CheckoutSpec) -> Result<Option<File>> {
        let path = self.repo_lock_path(spec);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        if try_lock_file(&file, LOCK_EX).is_err() {
            return Ok(None);
        }
        Ok(Some(file))
    }

    fn read_records(&self) -> Result<Metadata> {
        let _lock = self.metadata_lock()?;
        self.read_records_locked()
    }

    fn read_records_locked(&self) -> Result<Metadata> {
        let path = self.metadata_path();
        let Ok(mut file) = File::open(path) else {
            return Ok(Metadata::default());
        };
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        let mut metadata: Metadata =
            serde_json::from_str(&text).context("reading review checkout metadata")?;
        for record in &mut metadata.records {
            if record.kind == RecordKind::Legacy || record.git_admin_dir.is_some() {
                record.materialized = true;
            }
        }
        Ok(metadata)
    }

    fn write_records_locked(&self, metadata: &Metadata) -> Result<()> {
        fs::create_dir_all(self.state_root())?;
        let tmp = self
            .state_root()
            .join(format!("metadata.json.tmp-{}", std::process::id()));
        let text = serde_json::to_vec_pretty(metadata)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(&text)?;
        file.sync_all()?;
        drop(file);
        fs::rename(tmp, self.metadata_path())?;
        File::open(self.state_root())?.sync_all()?;
        Ok(())
    }

    fn with_metadata<R>(&self, f: impl FnOnce(&mut Metadata) -> Result<R>) -> Result<R> {
        let _lock = self.metadata_lock()?;
        let mut metadata = self.read_records_locked()?;
        let result = f(&mut metadata)?;
        self.write_records_locked(&metadata)?;
        Ok(result)
    }

    fn with_metadata_cancellable<R>(
        &self,
        cancel: &AtomicBool,
        f: impl FnOnce(&mut Metadata) -> Result<R>,
    ) -> Result<R> {
        let _lock = self.metadata_lock_cancellable(cancel)?;
        let mut metadata = self.read_records_locked()?;
        let result = f(&mut metadata)?;
        self.write_records_locked(&metadata)?;
        Ok(result)
    }

    fn update_record_locked(
        &self,
        metadata: &mut Metadata,
        spec: &CheckoutSpec,
        f: impl FnOnce(&mut Option<CheckoutRecord>),
    ) {
        Self::update_record_in_metadata(metadata, spec, f);
    }

    fn update_record_in_metadata(
        metadata: &mut Metadata,
        spec: &CheckoutSpec,
        f: impl FnOnce(&mut Option<CheckoutRecord>),
    ) {
        let mut record = metadata
            .records
            .iter()
            .position(|r| r.key == spec.key())
            .map(|ix| metadata.records.remove(ix));
        f(&mut record);
        if let Some(record) = record {
            metadata.records.push(record);
        }
    }

    fn remove_record(&self, spec: &CheckoutSpec) -> Result<()> {
        self.with_metadata(|metadata| {
            Self::remove_record_from_metadata(metadata, spec);
            Ok(())
        })
    }

    fn remove_record_locked(&self, metadata: &mut Metadata, spec: &CheckoutSpec) {
        Self::remove_record_from_metadata(metadata, spec);
    }

    fn remove_record_from_metadata(metadata: &mut Metadata, spec: &CheckoutSpec) {
        metadata.records.retain(|record| record.key != spec.key());
    }
}

impl ReviewCheckoutLease {
    pub(crate) fn head_oid(&self) -> &str {
        &self.inner.spec.head_oid
    }

    pub(crate) fn path(&self) -> Option<PathBuf> {
        self.inner
            .path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn operation(&self, _name: &'static str) -> Result<CheckoutOperation> {
        if self.inner.state.closing.load(Ordering::Acquire) {
            bail!("review checkout is closing");
        }
        let mut active = self
            .inner
            .state
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.inner.state.closing.load(Ordering::Acquire) {
            bail!("review checkout is closing");
        }
        *active += 1;
        Ok(CheckoutOperation {
            state: Arc::clone(&self.inner.state),
        })
    }

    pub(crate) fn mark_closing(&self) {
        self.inner.state.closing.store(true, Ordering::Release);
    }

    pub(crate) fn teardown(&self) -> Result<bool> {
        self.inner.manager.teardown(self)
    }
}

impl CheckoutOperation {
    pub(crate) fn cancel_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.state.closing)
    }
}

fn normalize_name(input: &str) -> Result<String> {
    let normalized = input.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || normalized == "."
        || normalized == ".."
        || !normalized
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("invalid GitHub owner or repository name: {input}");
    }
    Ok(normalized)
}

fn record_spec(record: &CheckoutRecord) -> Result<CheckoutSpec> {
    let owner = normalize_name(&record.owner)?;
    let repo = normalize_name(&record.repo)?;
    let head_oid = record.head_oid.to_ascii_lowercase();
    if owner != record.owner
        || repo != record.repo
        || head_oid != record.head_oid
        || !is_full_oid(&head_oid)
    {
        bail!(
            "review checkout record has non-normalized identity: {}",
            record.key
        );
    }
    let spec = CheckoutSpec {
        owner,
        repo,
        number: record.number,
        head_oid,
    };
    if record.key != spec.key() {
        bail!(
            "review checkout record key does not match its identity: {}",
            record.key
        );
    }
    Ok(spec)
}

fn legacy_name_matches(name: &str, spec: &CheckoutSpec) -> bool {
    let name = name
        .split_once(".tmp-clear-")
        .map(|(name, suffix)| (!suffix.is_empty()).then_some(name))
        .flatten()
        .unwrap_or(name);
    let Some((repo_key, rest)) = name.split_once("__pr") else {
        return false;
    };
    let Some((number, oid)) = rest.split_once("__") else {
        return false;
    };
    let Ok(number) = number.parse::<u64>() else {
        return false;
    };
    repo_key.eq_ignore_ascii_case(&spec.repo_key())
        && number == spec.number
        && oid.len() >= 12
        && oid.len() <= 40
        && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
        && spec.head_oid.starts_with(&oid.to_ascii_lowercase())
}

fn is_full_oid(input: &str) -> bool {
    input.len() == 40 && input.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn lock_file(file: &File, mode: i32) -> Result<()> {
    let result = unsafe { flock(file.as_raw_fd(), mode) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn try_lock_file(file: &File, mode: i32) -> Result<()> {
    lock_file(file, mode | LOCK_NB)
}

fn unlock_file(file: &File) -> Result<()> {
    lock_file(file, LOCK_UN)
}

fn command_with_env(command: &mut Command) -> &mut Command {
    for (key, value) in crate::cached_prs::git_env() {
        command.env(key, value);
    }
    command
}

fn run_command(command: &mut Command) -> Result<()> {
    let output = command_with_env(command).output()?;
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn check_cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        bail!("review checkout materialization cancelled");
    }
    Ok(())
}

fn check_acquire_cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        bail!("review checkout acquisition cancelled");
    }
    Ok(())
}

fn lock_file_cancellable(file: &File, mode: i32, cancel: &AtomicBool) -> Result<()> {
    loop {
        check_acquire_cancelled(cancel)?;
        let result = unsafe { flock(file.as_raw_fd(), mode | LOCK_NB) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(error.into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn terminate_process_group(child: &mut std::process::Child) -> Result<()> {
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    child.wait()?;
    Ok(())
}

fn run_command_cancellable(command: &mut Command, cancel: &AtomicBool) -> Result<()> {
    check_cancelled(cancel)?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    configure_process_group(command_with_env(command));
    let mut child = command.spawn()?;
    loop {
        if cancel.load(Ordering::Acquire) {
            terminate_process_group(&mut child)?;
            bail!("review checkout materialization cancelled");
        }
        if let Some(status) = child.try_wait()? {
            let output = child.wait_with_output()?;
            if status.success() {
                return Ok(());
            }
            bail!(
                "command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    command
}

fn run_git(dir: &Path, args: &[&str]) -> Result<()> {
    let mut command = git_command(dir, args);
    run_command(&mut command)
}

fn run_git_cancellable(dir: &Path, args: &[&str], cancel: &AtomicBool) -> Result<()> {
    let mut command = git_command(dir, args);
    run_command_cancellable(&mut command, cancel)
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = command_with_env(&mut git_command(dir, args)).output()?;
    if output.status.success() {
        Ok(String::from_utf8(output.stdout)?)
    } else {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn git_output_cancellable(dir: &Path, args: &[&str], cancel: &AtomicBool) -> Result<String> {
    check_cancelled(cancel)?;
    let mut command = git_command(dir, args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    configure_process_group(command_with_env(&mut command));
    let mut child = command.spawn()?;
    loop {
        if cancel.load(Ordering::Acquire) {
            terminate_process_group(&mut child)?;
            bail!("review checkout materialization cancelled");
        }
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            if output.status.success() {
                return Ok(String::from_utf8(output.stdout)?);
            }
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn registered_worktree_cancellable(
    base: &Path,
    path: &Path,
    head_oid: &str,
    cancel: &AtomicBool,
) -> Result<bool> {
    let top = git_output_cancellable(path, &["rev-parse", "--show-toplevel"], cancel)?;
    let head = git_output_cancellable(path, &["rev-parse", "HEAD"], cancel)?;
    let Ok(dot_git) = fs::symlink_metadata(path.join(".git")) else {
        return Ok(false);
    };
    Ok(dot_git.file_type().is_file()
        && PathBuf::from(top.trim()).canonicalize().ok() == path.canonicalize().ok()
        && head.trim() == head_oid
        && registered_paths_cancellable(base, cancel)?
            .iter()
            .any(|registered| registered.canonicalize().ok() == path.canonicalize().ok()))
}

fn registered_path(base: &Path, path: &Path) -> Result<bool> {
    match fs::symlink_metadata(base) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("managed base clone is a symlink: {}", base.display())
        }
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    }
    Ok(managed_registered_paths(base)?
        .iter()
        .any(|registered| same_path(registered, path)))
}

fn managed_registered_paths(base: &Path) -> Result<Vec<PathBuf>> {
    let base_metadata = fs::symlink_metadata(base)?;
    if base_metadata.file_type().is_symlink() {
        bail!("managed base clone is a symlink: {}", base.display());
    }
    if !base_metadata.file_type().is_dir() {
        bail!("managed base clone is not a directory: {}", base.display());
    }
    let git_dir = base.join(".git");
    let git_metadata = fs::symlink_metadata(&git_dir)?;
    if git_metadata.file_type().is_symlink() {
        bail!(
            "managed base Git directory is a symlink: {}",
            git_dir.display()
        );
    }
    if !git_metadata.file_type().is_dir() {
        bail!(
            "managed base Git directory is not a directory: {}",
            git_dir.display()
        );
    }
    let admin_root = git_dir.join("worktrees");
    let admin_root_exists = match fs::symlink_metadata(&admin_root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "managed Git worktree admin root is a symlink: {}",
                admin_root.display()
            )
        }
        Ok(metadata) if !metadata.file_type().is_dir() => {
            bail!(
                "managed Git worktree admin root is not a directory: {}",
                admin_root.display()
            )
        }
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };
    let mut paths = registered_paths(base)?;
    if !admin_root_exists {
        return Ok(paths);
    }
    let entries = match fs::read_dir(&admin_root) {
        Ok(entries) => entries,
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let admin = entry.path();
        let metadata = fs::symlink_metadata(&admin)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            bail!(
                "managed Git worktree admin entry is not a directory: {}",
                admin.display()
            );
        }
        let gitdir = admin.join("gitdir");
        let metadata = fs::symlink_metadata(&gitdir)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            bail!(
                "managed Git worktree backlink is not a regular file: {}",
                gitdir.display()
            );
        }
        let target = PathBuf::from(fs::read_to_string(gitdir)?.trim());
        let target = if target.is_absolute() {
            target
        } else {
            admin.join(target)
        };
        let Some(worktree) = target.parent() else {
            bail!(
                "managed Git worktree backlink has no parent: {}",
                target.display()
            );
        };
        if !paths.iter().any(|path| same_path(path, worktree)) {
            paths.push(worktree.to_path_buf());
        }
    }
    Ok(paths)
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right || canonicalize_with_missing(left) == canonicalize_with_missing(right)
}

fn canonicalize_with_missing(path: &Path) -> Option<PathBuf> {
    let mut suffix = Vec::new();
    let mut existing = path;
    while !existing.exists() {
        suffix.push(existing.file_name()?.to_owned());
        existing = existing.parent()?;
    }
    let mut canonical = existing.canonicalize().ok()?;
    for component in suffix.iter().rev() {
        canonical.push(component);
    }
    Some(canonical)
}

fn same_remote(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    if let (Some(expected), Some(actual)) = (
        crate::cached_prs::parse_github_owner_repo(expected),
        crate::cached_prs::parse_github_owner_repo(actual),
    ) {
        return expected.0.eq_ignore_ascii_case(&actual.0)
            && expected.1.eq_ignore_ascii_case(&actual.1);
    }
    let expected_path = Path::new(expected.strip_prefix("file://").unwrap_or(expected));
    let actual_path = Path::new(actual.strip_prefix("file://").unwrap_or(actual));
    expected_path
        .canonicalize()
        .ok()
        .zip(actual_path.canonicalize().ok())
        .is_some_and(|(expected, actual)| expected == actual)
}

fn registered_paths(base: &Path) -> Result<Vec<PathBuf>> {
    let output = git_output(base, &["worktree", "list", "--porcelain"])?;
    Ok(output
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

fn registered_paths_cancellable(base: &Path, cancel: &AtomicBool) -> Result<Vec<PathBuf>> {
    let output = git_output_cancellable(base, &["worktree", "list", "--porcelain"], cancel)?;
    Ok(output
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

fn infer_record(_base: &Path, path: &Path) -> Option<CheckoutRecord> {
    let name = path.file_name()?.to_string_lossy();
    let (repo_key, rest) = name.split_once("__pr")?;
    let (number, oid) = rest.split_once("__")?;
    let number = number.parse().ok()?;
    let head_oid = git_output(path, &["rev-parse", "HEAD"])
        .ok()?
        .trim()
        .to_string();
    if oid.len() < 12 || !is_full_oid(&head_oid) || !oid.starts_with(&head_oid[..12]) {
        return None;
    }
    let (owner, repo) = repo_key.split_once("__")?;
    let spec = CheckoutSpec {
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        head_oid,
    };
    Some(CheckoutRecord {
        key: spec.key(),
        owner: spec.owner,
        repo: spec.repo,
        number,
        head_oid: spec.head_oid,
        path: path.display().to_string(),
        git_admin_dir: git_output(path, &["rev-parse", "--absolute-git-dir"])
            .ok()
            .map(|s| s.trim().to_string()),
        created_secs: now_secs(),
        last_used_secs: now_secs(),
        pending_cleanup: true,
        storage_teardown_complete: false,
        storage_initialized_without_id: false,
        materialized: true,
        kind: RecordKind::Linked,
    })
}

fn legacy_identity(path: &Path) -> Result<Option<(String, String, u64, String)>> {
    let Some(file_name) = path.file_name() else {
        return Ok(None);
    };
    let name = file_name.to_string_lossy();
    let name = name
        .split_once(".tmp-clear-")
        .map(|(name, _)| name)
        .unwrap_or(&name);
    let Some((repo_key, rest)) = name.split_once("__pr") else {
        return Ok(None);
    };
    let Some((number, oid)) = rest.split_once("__") else {
        return Ok(None);
    };
    let Ok(number) = number.parse() else {
        return Ok(None);
    };
    if oid.len() < 12 || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let Some((owner, repo)) = repo_key.split_once("__") else {
        return Ok(None);
    };
    let remote = git_output(path, &["config", "--get", "remote.origin.url"])
        .with_context(|| format!("reading legacy checkout origin {}", path.display()))?;
    let Some((remote_owner, remote_repo)) =
        crate::cached_prs::parse_github_owner_repo(remote.trim())
    else {
        return Ok(None);
    };
    if !remote_owner.eq_ignore_ascii_case(owner) || !remote_repo.eq_ignore_ascii_case(repo) {
        return Ok(None);
    }
    let head_oid = git_output(path, &["rev-parse", "HEAD"])
        .with_context(|| format!("reading legacy checkout HEAD {}", path.display()))?
        .trim()
        .to_string();
    if !is_full_oid(&head_oid) {
        bail!(
            "legacy checkout HEAD is not a full commit id: {}",
            path.display()
        );
    }
    Ok(Some((
        owner.to_ascii_lowercase(),
        repo.to_ascii_lowercase(),
        number,
        head_oid,
    )))
}

#[cfg(test)]
#[path = "review_checkouts_recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
#[path = "review_checkouts_safety_tests.rs"]
mod safety_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_path(label: &str) -> PathBuf {
        let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "lgtm-review-checkouts-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn git(dir: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        command
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        assert!(command.status().unwrap().success(), "git {:?}", args);
    }

    #[test]
    fn names_are_case_insensitive_and_oids_must_be_full() {
        let loc = gh::PrLocator {
            owner: "Acme".into(),
            repo: "Thing".into(),
            number: 3,
        };
        let spec = CheckoutSpec::new(&loc, &"A".repeat(40)).unwrap();
        assert_eq!(spec.key(), "acme__thing__pr3__aaaaaaaaaaaa");
        assert!(CheckoutSpec::new(&loc, "deadbeef").is_err());
    }

    #[test]
    fn materializes_one_base_and_two_registered_heads() {
        let remote = temp_path("remote");
        git(&remote, &["init", "--bare"]);
        let seed = temp_path("seed");
        git(&seed, &["init"]);
        fs::write(seed.join("file"), "one\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-m", "one"]);
        git(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&seed, &["push", "origin", "HEAD:main"]);
        fs::write(seed.join("file"), "two\n").unwrap();
        git(&seed, &["commit", "-am", "two"]);
        let second = git_output(&seed, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        git(&seed, &["push", "origin", "HEAD:refs/pull/7/head"]);
        fs::write(seed.join("file"), "three\n").unwrap();
        git(&seed, &["commit", "-am", "three"]);
        let third = git_output(&seed, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        git(&seed, &["push", "origin", "HEAD:refs/pull/8/head"]);
        let root = temp_path("root");
        let manager =
            ReviewCheckoutManager::at_root(root.join("cache"), Some(remote.display().to_string()));
        let loc = gh::PrLocator {
            owner: "Acme".into(),
            repo: "Thing".into(),
            number: 7,
        };
        let first_lease = manager.acquire(&loc, &second).unwrap();
        let first_path = manager.materialize(&first_lease).unwrap();
        assert!(first_path.join(".git").is_file());
        assert_eq!(
            git_output(&first_path, &["rev-parse", "HEAD"])
                .unwrap()
                .trim(),
            second
        );
        let second_lease = manager.acquire(&loc, &second).unwrap();
        assert_eq!(manager.materialize(&second_lease).unwrap(), first_path);
        let second_head_loc = gh::PrLocator {
            owner: "acme".into(),
            repo: "thing".into(),
            number: 8,
        };
        let second_head_lease = manager.acquire(&second_head_loc, &third).unwrap();
        let second_head_path = manager.materialize(&second_head_lease).unwrap();
        assert_ne!(second_head_path, first_path);
        drop(second_head_lease);
        manager
            .with_metadata(|metadata| {
                metadata
                    .records
                    .iter_mut()
                    .find(|record| record.number == 8)
                    .unwrap()
                    .last_used_secs = 0;
                Ok(())
            })
            .unwrap();
        ReviewCheckoutManager::at_root(root.join("cache"), Some(remote.display().to_string()))
            .startup()
            .unwrap();
        assert!(!second_head_path.exists());
        assert!(manager.teardown(&first_lease).unwrap() == false);
        drop(second_lease);
        assert!(manager.teardown(&first_lease).unwrap());
        assert!(!first_path.exists());
        assert!(root.join("cache/repos/acme__thing").is_dir());
        for (number, suffix) in [(9, ""), (10, ".tmp-clear-999-1")] {
            let legacy = manager
                .worktrees_root()
                .join(format!("acme__thing__pr{number}__{}{suffix}", &third[..12]));
            let mut clone = Command::new("git");
            clone.args(["clone", remote.to_str().unwrap(), legacy.to_str().unwrap()]);
            assert!(clone.status().unwrap().success());
            git(&legacy, &["fetch", "origin", "refs/pull/8/head"]);
            git(
                &legacy,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    "https://github.com/acme/thing.git",
                ],
            );
            git(&legacy, &["checkout", "--detach", &third]);
        }
        manager
            .with_metadata(|metadata| {
                metadata.migration_complete = false;
                Ok(())
            })
            .unwrap();
        ReviewCheckoutManager::at_root(root.join("cache"), Some(remote.display().to_string()))
            .startup()
            .unwrap();
        assert!(manager
            .worktrees_root()
            .read_dir()
            .unwrap()
            .next()
            .is_none());
        let _ = fs::remove_dir_all(remote);
        let _ = fs::remove_dir_all(seed);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn operation_barrier_rejects_new_work_after_close() {
        let root = temp_path("barrier");
        let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
        let loc = gh::PrLocator {
            owner: "a".into(),
            repo: "b".into(),
            number: 1,
        };
        let lease = manager.acquire(&loc, &"a".repeat(40)).unwrap();
        let operation = lease.operation("request").unwrap();
        lease.mark_closing();
        assert!(lease.operation("late").is_err());
        drop(operation);
    }

    #[test]
    fn startup_keeps_missing_materialized_identity_without_teardown_proof() {
        let root = temp_path("missing");
        let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
        let spec = CheckoutSpec {
            owner: "acme".into(),
            repo: "thing".into(),
            number: 1,
            head_oid: "a".repeat(40),
        };
        let path = manager.worktrees_root().join(spec.key());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        manager
            .with_metadata(|metadata| {
                metadata.records.push(CheckoutRecord {
                    key: spec.key(),
                    owner: spec.owner.clone(),
                    repo: spec.repo.clone(),
                    number: spec.number,
                    head_oid: spec.head_oid.clone(),
                    path: path.display().to_string(),
                    git_admin_dir: Some("/managed/admin-identity".into()),
                    created_secs: 0,
                    last_used_secs: 0,
                    pending_cleanup: false,
                    storage_teardown_complete: false,
                    storage_initialized_without_id: false,
                    materialized: true,
                    kind: RecordKind::Linked,
                });
                Ok(())
            })
            .unwrap();
        manager.startup().unwrap();
        let record = manager
            .read_records()
            .unwrap()
            .records
            .into_iter()
            .find(|record| record.key == spec.key())
            .unwrap();
        assert!(record.pending_cleanup);
        assert_eq!(
            record.git_admin_dir.as_deref(),
            Some("/managed/admin-identity")
        );
        let _ = fs::remove_dir_all(root);
    }
}
