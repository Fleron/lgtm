use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

fn temp_root(label: &str) -> PathBuf {
    let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "lgtm-review-checkouts-safety-{label}-{}-{id}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {:?}", args);
}

fn spec(number: u64, head: &str) -> CheckoutSpec {
    CheckoutSpec {
        owner: "acme".into(),
        repo: "thing".into(),
        number,
        head_oid: head.into(),
    }
}

fn record(spec: &CheckoutSpec, path: &Path, kind: RecordKind) -> CheckoutRecord {
    CheckoutRecord {
        key: spec.key(),
        owner: spec.owner.clone(),
        repo: spec.repo.clone(),
        number: spec.number,
        head_oid: spec.head_oid.clone(),
        path: path.display().to_string(),
        git_admin_dir: None,
        created_secs: 1,
        last_used_secs: 1,
        pending_cleanup: false,
        storage_teardown_complete: false,
        storage_initialized_without_id: false,
        materialized: true,
        kind,
    }
}

fn create_legacy_checkout(
    manager: &ReviewCheckoutManager,
    spec: &CheckoutSpec,
) -> (PathBuf, CheckoutRecord) {
    let path = manager.worktrees_root().join(format!(
        "{}__pr{}__{}",
        spec.repo_key(),
        spec.number,
        &spec.head_oid[..12]
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::create_dir_all(&path).unwrap();
    git(&path, &["init"]);
    git(
        &path,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/thing.git",
        ],
    );
    fs::create_dir(path.join("leap")).unwrap();
    let record = record(spec, &path, RecordKind::Legacy);
    (path, record)
}

#[test]
fn materialization_cancellation_kills_hanging_git() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("cancel-child");
        let fake_bin = root.join("bin");
        fs::create_dir_all(&fake_bin).unwrap();
        let started = root.join("git-started");
        let fake_git = fake_bin.join("git");
        fs::write(
            &fake_git,
            "#!/bin/sh\nprintf started > \"$FAKE_GIT_STARTED\"\nsleep 4\n",
        )
        .unwrap();
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let elapsed = Instant::now();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "review_checkouts::safety_tests::materialization_cancellation_child",
                "--ignored",
                "--nocapture",
            ])
            .env(
                "PATH",
                format!("{}:{}", fake_bin.display(), old_path.to_string_lossy()),
            )
            .env("FAKE_GIT_STARTED", &started)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(started.exists());
        assert!(
            elapsed.elapsed() < Duration::from_secs(3),
            "Git cancellation took {:?}",
            elapsed.elapsed()
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[test]
fn cancellable_acquire_aborts_while_startup_metadata_lock_is_held() {
    let root = temp_root("acquire-cancel-lock");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    fs::create_dir_all(manager.state_root()).unwrap();
    let held = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(manager.state_lock_path())
        .unwrap();
    lock_file(&held, LOCK_EX).unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();
    let worker_manager = manager.clone();
    let worker_cancel = Arc::clone(&cancel);
    let worker = std::thread::spawn(move || {
        let loc = gh::PrLocator {
            owner: "acme".into(),
            repo: "thing".into(),
            number: 1,
        };
        sender
            .send(worker_manager.acquire_cancellable(&loc, &"a".repeat(40), worker_cancel.as_ref()))
            .unwrap();
    });

    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    cancel.store(true, Ordering::Release);
    let result = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(result.is_err(), "canceled acquire unexpectedly succeeded");
    worker.join().unwrap();
    drop(held);
    manager.startup().unwrap();
    let _ = fs::remove_dir_all(root);
}

#[test]
fn materialization_cancellation_aborts_while_repo_lock_is_held() {
    let root = temp_root("materialize-cancel-repo-lock");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();
    let spec = spec(12, &"a".repeat(40));
    let locator = gh::PrLocator {
        owner: "acme".into(),
        repo: "thing".into(),
        number: spec.number,
    };
    let lease = manager.acquire(&locator, &spec.head_oid).unwrap();
    let held = manager.repo_lock(&spec).unwrap();
    let (sender, receiver) = mpsc::channel();
    let worker_manager = manager.clone();
    let worker_lease = lease.clone();
    let worker = std::thread::spawn(move || {
        sender
            .send(worker_manager.materialize(&worker_lease).is_err())
            .unwrap();
    });
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    lease.mark_closing();
    assert!(receiver.recv_timeout(Duration::from_secs(1)).unwrap());
    worker.join().unwrap();
    drop(held);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn materialization_cancellation_aborts_while_metadata_lock_is_held() {
    let root = temp_root("materialize-cancel-metadata-lock");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();
    let spec = spec(13, &"b".repeat(40));
    let locator = gh::PrLocator {
        owner: "acme".into(),
        repo: "thing".into(),
        number: spec.number,
    };
    let lease = manager.acquire(&locator, &spec.head_oid).unwrap();
    let held = manager.metadata_lock().unwrap();
    let (sender, receiver) = mpsc::channel();
    let worker_manager = manager.clone();
    let worker_lease = lease.clone();
    let worker = std::thread::spawn(move || {
        sender
            .send(worker_manager.materialize(&worker_lease).is_err())
            .unwrap();
    });
    assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
    lease.mark_closing();
    assert!(receiver.recv_timeout(Duration::from_secs(1)).unwrap());
    worker.join().unwrap();
    drop(held);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn acquire_preserves_storage_checkpoint_before_materialization() {
    let root = temp_root("acquire-preserves-checkpoint");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();
    let spec = spec(2, &"b".repeat(40));
    let path = manager.worktrees_root().join(spec.key());
    manager
        .with_metadata(|metadata| {
            metadata.records.push(CheckoutRecord {
                key: spec.key(),
                owner: spec.owner.clone(),
                repo: spec.repo.clone(),
                number: spec.number,
                head_oid: spec.head_oid.clone(),
                path: path.display().to_string(),
                git_admin_dir: None,
                created_secs: 1,
                last_used_secs: 1,
                pending_cleanup: true,
                storage_teardown_complete: true,
                storage_initialized_without_id: false,
                materialized: true,
                kind: RecordKind::Linked,
            });
            Ok(())
        })
        .unwrap();
    let loc = gh::PrLocator {
        owner: "acme".into(),
        repo: "thing".into(),
        number: 2,
    };
    let lease = manager.acquire(&loc, &spec.head_oid).unwrap();
    let saved = manager
        .read_records()
        .unwrap()
        .records
        .into_iter()
        .find(|record| record.key == spec.key())
        .unwrap();
    assert!(saved.storage_teardown_complete);
    assert!(manager.teardown(&lease).unwrap());
    assert!(manager
        .read_records()
        .unwrap()
        .records
        .into_iter()
        .all(|record| record.key != spec.key()));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn canceled_acquired_checkout_is_recoverable_before_materialization() {
    let root = temp_root("canceled-after-acquire");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    let loc = gh::PrLocator {
        owner: "acme".into(),
        repo: "thing".into(),
        number: 3,
    };
    let head = "c".repeat(40);
    let tasks = Arc::new(crate::app::PendingCheckoutTasks::default());
    let pending = tasks.begin(3, 1).unwrap();
    let lease = manager.acquire(&loc, &head).unwrap();
    pending.cancel_token().store(true, Ordering::Release);
    pending.register_lease(&lease);
    assert!(pending.is_cancelled());
    assert!(manager.teardown(&lease).unwrap());
    assert!(manager
        .read_records()
        .unwrap()
        .records
        .into_iter()
        .all(|record| record.head_oid != head));
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[ignore]
#[test]
fn materialization_cancellation_child() {
    use std::sync::mpsc;

    let marker = PathBuf::from(std::env::var_os("FAKE_GIT_STARTED").unwrap());
    let root = temp_root("cancel-worker");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), Some("unused".into()));
    let loc = gh::PrLocator {
        owner: "acme".into(),
        repo: "thing".into(),
        number: 1,
    };
    let lease = manager.acquire(&loc, &"a".repeat(40)).unwrap();
    let (sender, receiver) = mpsc::channel();
    let worker = lease.clone();
    let handle = std::thread::spawn(move || {
        sender.send(manager.materialize(&worker).is_err()).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    lease.mark_closing();
    let killed_at = Instant::now();
    let result = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    handle.join().unwrap();
    assert!(marker.exists());
    assert!(result);
    assert!(
        killed_at.elapsed() < Duration::from_secs(2),
        "Git cancellation took {:?}",
        killed_at.elapsed()
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn legacy_record_is_not_overwritten_by_acquire() {
    let root = temp_root("missing-package");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();
    let spec = spec(1, &"a".repeat(40));
    let (path, record) = create_legacy_checkout(&manager, &spec);
    fs::write(path.join(".git").join("leap-environment-id"), "environment").unwrap();
    manager
        .with_metadata(|metadata| {
            metadata.records.push(record);
            Ok(())
        })
        .unwrap();
    let loc = gh::PrLocator {
        owner: "acme".into(),
        repo: "thing".into(),
        number: 1,
    };
    let error = manager
        .acquire(&loc, &spec.head_oid)
        .err()
        .expect("acquire unexpectedly succeeded");
    assert!(error
        .to_string()
        .contains("legacy review checkout still requires migration"));
    let saved = manager
        .read_records()
        .unwrap()
        .records
        .into_iter()
        .find(|saved| saved.key == spec.key())
        .unwrap();
    assert_eq!(saved.kind, RecordKind::Legacy);
    assert_eq!(saved.path, path.display().to_string());
    assert!(saved.git_admin_dir.is_none());
    assert!(!saved.pending_cleanup);
    assert!(!saved.storage_teardown_complete);
    assert!(path.exists());
    assert!(path.join(".git").join("leap-environment-id").exists());
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn symlinked_leap_git_is_rejected_before_storage_validation() {
    use std::os::unix::fs::symlink;

    let root = temp_root("symlink");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();
    let spec = spec(2, &"b".repeat(40));
    let (path, record) = create_legacy_checkout(&manager, &spec);
    let outside = root.join("outside");
    fs::create_dir_all(outside.join(".git")).unwrap();
    symlink(outside.join(".git"), path.join("leap").join(".git")).unwrap();
    let mut metadata = Metadata {
        records: vec![record.clone()],
        migration_complete: true,
    };
    let error = manager
        .destroy_storage_locked(&spec, &path, &record, &mut metadata)
        .unwrap_err();
    assert!(error.to_string().contains("symlink"));
    assert!(!record.storage_initialized_without_id);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn record_identity_and_path_are_bound_while_head_mismatch_is_tolerated_for_owned_worktree() {
    let root = temp_root("identity");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();
    fs::create_dir_all(manager.worktrees_root()).unwrap();
    let first = spec(3, &"c".repeat(40));
    let second = spec(4, &"d".repeat(40));
    let swapped = record(
        &first,
        &manager.worktrees_root().join(second.key()),
        RecordKind::Linked,
    );
    assert!(manager.validate_record_path(&swapped).is_err());
    let mut malformed = swapped.clone();
    malformed.path = manager
        .worktrees_root()
        .join(first.key())
        .display()
        .to_string();
    malformed.owner = "Acme".into();
    assert!(manager.validate_record_path(&malformed).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn unknown_worktree_registration_blocks_global_prune_but_selected_teardown_finishes() {
    let root = temp_root("prune");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();
    let source = root.join("source");
    fs::create_dir_all(&source).unwrap();
    git(&source, &["init"]);
    fs::write(source.join("file"), "content").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "initial"]);
    let base = manager.repos_root().join("acme__thing");
    fs::create_dir_all(base.parent().unwrap()).unwrap();
    git(&manager.repos_root(), &["init", "--bare", "remote"]);
    git(
        &source,
        &[
            "remote",
            "add",
            "origin",
            manager.repos_root().join("remote").to_str().unwrap(),
        ],
    );
    git(&source, &["push", "origin", "HEAD:main"]);
    git(&manager.repos_root(), &["clone", "remote", "acme__thing"]);
    let head = git_output(&base, &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let selected = spec(5, &head);
    let unknown = spec(6, &"d".repeat(40));
    let selected_path = manager.worktrees_root().join(selected.key());
    let unknown_path = manager.worktrees_root().join(unknown.key());
    git(
        &base,
        &[
            "worktree",
            "add",
            "--detach",
            selected_path.to_str().unwrap(),
            &head,
        ],
    );
    git(
        &base,
        &[
            "worktree",
            "add",
            "--detach",
            unknown_path.to_str().unwrap(),
            &head,
        ],
    );
    let unknown_admin = fs::read_dir(base.join(".git").join("worktrees"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|admin| {
            let target = fs::read_to_string(admin.join("gitdir")).unwrap();
            let target = PathBuf::from(target.trim());
            same_path(&target, &unknown_path.join(".git"))
        })
        .unwrap();
    assert!(manager
        .validate_record_path(&record(&unknown, &unknown_path, RecordKind::Linked))
        .is_ok());
    fs::remove_dir_all(&unknown_path).unwrap();
    assert!(registered_path(&base, &unknown_path).unwrap());
    git(&base, &["config", "gc.worktreePruneExpire", "now"]);
    manager
        .with_metadata(|metadata| {
            metadata
                .records
                .push(record(&selected, &selected_path, RecordKind::Linked));
            Ok(())
        })
        .unwrap();
    let loc = gh::PrLocator {
        owner: "acme".into(),
        repo: "thing".into(),
        number: selected.number,
    };
    let lease = manager.acquire(&loc, &head).unwrap();
    assert!(manager.teardown(&lease).unwrap());
    assert!(!selected_path.exists());
    assert!(unknown_admin.exists());
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn registration_scan_errors_keep_cleanup_pending() {
    use std::os::unix::fs::symlink;

    for malformed_admin in [false, true] {
        let root = temp_root(if malformed_admin {
            "registration-malformed-admin"
        } else {
            "registration-git-error"
        });
        let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
        manager.startup().unwrap();
        let spec = spec(if malformed_admin { 8 } else { 7 }, &"e".repeat(40));
        let base = manager.repos_root().join(spec.repo_key());
        fs::create_dir_all(&base).unwrap();
        if malformed_admin {
            git(&base, &["init"]);
            let admin_root = base.join(".git").join("worktrees");
            fs::create_dir_all(&admin_root).unwrap();
            let outside = root.join("outside-admin");
            fs::create_dir_all(&outside).unwrap();
            symlink(&outside, admin_root.join("unknown")).unwrap();
        } else {
            fs::create_dir_all(base.join(".git")).unwrap();
        }
        let path = manager.worktrees_root().join(spec.key());
        let record = record(&spec, &path, RecordKind::Linked);
        manager
            .with_metadata(|metadata| {
                metadata.records.push(record);
                Ok(())
            })
            .unwrap();
        let loc = gh::PrLocator {
            owner: "acme".into(),
            repo: "thing".into(),
            number: spec.number,
        };
        let lease = manager.acquire(&loc, &spec.head_oid).unwrap();
        manager
            .with_metadata(|metadata| {
                metadata
                    .records
                    .iter_mut()
                    .find(|record| record.key == spec.key())
                    .unwrap()
                    .storage_teardown_complete = true;
                Ok(())
            })
            .unwrap();
        assert!(manager.teardown(&lease).is_err());
        let saved = manager
            .read_records()
            .unwrap()
            .records
            .into_iter()
            .find(|record| record.key == spec.key())
            .unwrap();
        assert!(saved.pending_cleanup);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(unix)]
#[test]
fn symlinked_base_git_layout_is_rejected_without_touching_external_admin() {
    use std::os::unix::fs::symlink;

    let root = temp_root("base-layout");
    let spec = spec(9, &"f".repeat(40));
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();

    let linked_git = root.join("external-git");
    fs::create_dir_all(&linked_git).unwrap();
    fs::write(linked_git.join("leap-environment-id"), "external").unwrap();
    let linked_base = manager.repos_root().join(spec.repo_key());
    fs::create_dir_all(linked_base.parent().unwrap()).unwrap();
    fs::create_dir_all(&linked_base).unwrap();
    symlink(&linked_git, linked_base.join(".git")).unwrap();
    let error = manager
        .ensure_base(&spec, &AtomicBool::new(false))
        .unwrap_err();
    assert!(error.to_string().contains("symlink"));
    assert_eq!(
        fs::read_to_string(linked_git.join("leap-environment-id")).unwrap(),
        "external"
    );

    let worktree_base = manager.repos_root().join("acme__other");
    fs::create_dir_all(&worktree_base).unwrap();
    git(&worktree_base, &["init"]);
    let worktrees = worktree_base.join(".git").join("worktrees");
    let _ = fs::remove_dir_all(&worktrees);
    let external_admin = root.join("external-worktrees");
    fs::create_dir_all(&external_admin).unwrap();
    fs::write(external_admin.join("leap-environment-id"), "external-admin").unwrap();
    symlink(&external_admin, &worktrees).unwrap();
    let other_spec = CheckoutSpec {
        owner: "acme".into(),
        repo: "other".into(),
        number: 10,
        head_oid: "1".repeat(40),
    };
    let error = manager
        .ensure_base(&other_spec, &AtomicBool::new(false))
        .unwrap_err();
    assert!(error.to_string().contains("symlink"));
    assert_eq!(
        fs::read_to_string(external_admin.join("leap-environment-id")).unwrap(),
        "external-admin"
    );
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn symlinked_managed_roots_are_rejected_without_touching_external_checkout() {
    use std::os::unix::fs::symlink;

    for root_kind in ["repos", "worktrees"] {
        let root = temp_root(&format!("managed-root-{root_kind}"));
        let cache = root.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let manager = ReviewCheckoutManager::at_root(cache, None);
        let external = root.join("external");
        fs::create_dir_all(&external).unwrap();
        symlink(&external, root.join("cache").join(root_kind)).unwrap();

        let marker = if root_kind == "repos" {
            let base = external.join("acme__thing");
            fs::create_dir_all(&base).unwrap();
            git(&base, &["init"]);
            let marker = base.join(".git").join("leap-environment-id");
            fs::write(&marker, "external-repo").unwrap();
            marker
        } else {
            let checkout = external.join("acme__thing__pr1__aaaaaaaaaaaa");
            fs::create_dir_all(&checkout).unwrap();
            git(&checkout, &["init"]);
            let marker = checkout.join(".git").join("leap-environment-id");
            fs::write(&marker, "external-worktree").unwrap();
            marker
        };
        let expected = fs::read_to_string(&marker).unwrap();
        assert!(
            manager.startup().is_err(),
            "accepted symlinked {root_kind} root"
        );
        assert_eq!(fs::read_to_string(&marker).unwrap(), expected);
        assert!(external.exists());
        let _ = fs::remove_dir_all(root);
    }

    let root = temp_root("managed-root-parent-symlink");
    let real_parent = root.join("real-parent");
    fs::create_dir_all(&real_parent).unwrap();
    let linked_parent = root.join("linked-parent");
    symlink(&real_parent, &linked_parent).unwrap();
    let manager = ReviewCheckoutManager::at_root(linked_parent.join("cache"), None);
    manager.startup().unwrap();
    assert!(manager.repos_root().is_dir());
    assert!(manager.worktrees_root().is_dir());
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn migration_scan_permission_errors_leave_migration_incomplete() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_root("migration-scan-error");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();
    manager
        .with_metadata(|metadata| {
            metadata.migration_complete = false;
            Ok(())
        })
        .unwrap();
    let worktrees = manager.worktrees_root();
    let original_mode = fs::metadata(&worktrees).unwrap().permissions().mode();
    fs::set_permissions(&worktrees, fs::Permissions::from_mode(0)).unwrap();
    if fs::read_dir(&worktrees).is_ok() {
        fs::set_permissions(&worktrees, fs::Permissions::from_mode(original_mode)).unwrap();
        let _ = fs::remove_dir_all(root);
        return;
    }
    manager.migrate_legacy_locked().unwrap();
    fs::set_permissions(&worktrees, fs::Permissions::from_mode(original_mode)).unwrap();
    assert!(!manager.read_records().unwrap().migration_complete);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn legacy_identity_probe_errors_keep_migration_incomplete_until_repaired() {
    let root = temp_root("legacy-identity-error");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();
    let spec = spec(11, &"a".repeat(40));
    let (path, _) = create_legacy_checkout(&manager, &spec);
    git(&path, &["config", "user.name", "safety-test"]);
    git(&path, &["config", "user.email", "safety-test@example.com"]);
    fs::write(path.join("README.md"), "legacy\n").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-m", "legacy"]);
    let head = git_output(&path, &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let repaired_path = manager.worktrees_root().join(format!(
        "{}__pr{}__{}",
        spec.repo_key(),
        spec.number,
        &head[..12]
    ));
    fs::rename(&path, &repaired_path).unwrap();
    let config = repaired_path.join(".git/config");
    let config_bytes = fs::read(&config).unwrap();
    fs::remove_file(&config).unwrap();

    manager
        .with_metadata(|metadata| {
            metadata.migration_complete = false;
            Ok(())
        })
        .unwrap();
    manager.migrate_legacy_locked().unwrap();
    assert!(!manager.read_records().unwrap().migration_complete);
    assert!(repaired_path.exists());

    fs::write(&config, config_bytes).unwrap();
    manager.migrate_legacy_locked().unwrap();
    assert!(manager.read_records().unwrap().migration_complete);
    assert!(!repaired_path.exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn reconcile_repository_scan_errors_defer_startup() {
    let root = temp_root("reconcile-scan-error");
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();
    let repos = manager.repos_root();
    fs::remove_dir_all(&repos).unwrap();
    fs::write(&repos, "not a directory").unwrap();
    assert!(manager.reconcile_records_locked().is_err());
    let _ = fs::remove_dir_all(root);
}
