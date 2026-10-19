use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
static NEXT_RECOVERY_DIR: AtomicU64 = AtomicU64::new(0);
fn temp_root(label: &str) -> PathBuf {
    let nonce = NEXT_RECOVERY_DIR.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "lgtm-review-checkout-recovery-{label}-{}-{nonce}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}
fn init_repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "--quiet"]);
    git(path, &["config", "user.name", "recovery-test"]);
    git(path, &["config", "user.email", "recovery-test@example.com"]);
}
fn git(path: &Path, args: &[&str]) {
    run_git(path, args).unwrap_or_else(|err| panic!("git {args:?}: {err:#}"));
}
fn git_text(path: &Path, args: &[&str]) -> String {
    git_output(path, args).unwrap_or_else(|err| panic!("git {args:?}: {err:#}"))
}
fn commit(path: &Path, file: &str, contents: &str, message: &str) -> String {
    let file = path.join(file);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(file, contents).unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "--quiet", "-m", message]);
    git_text(path, &["rev-parse", "HEAD"]).trim().to_owned()
}
fn bare_remote(root: &Path, leap_package: Option<&str>) -> (PathBuf, PathBuf, String) {
    let remote = root.join("remote.git");
    let seed = root.join("seed");
    fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--bare", "--quiet"]);
    init_repo(&seed);
    if let Some(package) = leap_package {
        fs::create_dir_all(seed.join("leap")).unwrap();
        fs::write(seed.join("leap/package.json"), package).unwrap();
    }
    let head = commit(&seed, "README.md", "seed\n", "seed");
    git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&seed, &["push", "--quiet", "origin", "HEAD:main"]);
    (remote, seed, head)
}
fn set_pull_head(remote: &Path, number: u64, head: &str) {
    let reference = format!("refs/pull/{number}/head");
    git(remote, &["update-ref", &reference, head]);
}
fn locator(number: u64) -> gh::PrLocator {
    gh::PrLocator {
        owner: "Acme".into(),
        repo: "Thing".into(),
        number,
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
        created_secs: 0,
        last_used_secs: 0,
        pending_cleanup: false,
        storage_teardown_complete: false,
        storage_initialized_without_id: false,
        materialized: true,
        kind,
    }
}

fn write_record(manager: &ReviewCheckoutManager, value: CheckoutRecord, migration_complete: bool) {
    manager
        .with_metadata(|metadata| {
            metadata.records.push(value);
            metadata.migration_complete = migration_complete;
            Ok(())
        })
        .unwrap();
}

fn clone_legacy(remote: &Path, path: &Path, origin: &str) {
    let mut command = Command::new("git");
    command.args([
        "clone",
        "--quiet",
        remote.to_str().unwrap(),
        path.to_str().unwrap(),
    ]);
    run_command(&mut command).unwrap();
    git(path, &["remote", "set-url", "origin", origin]);
}

#[test]
fn shared_base_fetches_new_head_and_teardown_waits_for_active_operation() {
    let root = temp_root("shared");
    let (remote, seed, first_head) = bare_remote(&root, None);
    set_pull_head(&remote, 7, &first_head);
    let cache = root.join("cache");
    let manager = ReviewCheckoutManager::at_root(cache, Some(remote.display().to_string()));
    let first = manager.acquire(&locator(7), &first_head).unwrap();
    let first_path = manager.materialize(&first).unwrap();
    let shared = manager.acquire(&locator(7), &first_head).unwrap();
    assert_eq!(manager.materialize(&shared).unwrap(), first_path);
    assert!(!first.teardown().unwrap());

    let operation = shared.operation("read").unwrap();
    let (finished_tx, finished_rx) = mpsc::channel();
    let closing = shared.clone();
    let thread = thread::spawn(move || finished_tx.send(closing.teardown()).unwrap());
    assert!(finished_rx
        .recv_timeout(Duration::from_millis(250))
        .is_err());
    drop(operation);
    assert!(finished_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap());
    thread.join().unwrap();
    assert!(!first_path.exists());

    let second_head = commit(&seed, "README.md", "updated\n", "updated");
    git(&seed, &["push", "--quiet", "origin", "HEAD:main"]);
    set_pull_head(&remote, 7, &second_head);

    let second_head_lease = manager.acquire(&locator(7), &second_head).unwrap();
    let second_path = manager.materialize(&second_head_lease).unwrap();
    assert_ne!(first_path, second_path);
    assert_eq!(
        git_text(&second_path, &["rev-parse", "HEAD"]).trim(),
        second_head
    );
    assert!(manager.repos_root().join("acme__thing/.git").is_dir());

    assert!(second_head_lease.teardown().unwrap());
    assert!(!second_path.exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn failed_legacy_migration_blocks_acquire_until_cleanup() {
    let root = temp_root("legacy-acquire-blocked");
    let (remote, _seed, head) = bare_remote(&root, None);
    let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
    manager.startup().unwrap();

    let legacy_path = manager
        .worktrees_root()
        .join(format!("Acme__Thing__pr7__{}", &head[..12]));
    clone_legacy(&remote, &legacy_path, "https://github.com/Acme/Thing.git");
    git(&legacy_path, &["checkout", "--detach", &head]);
    let admin = PathBuf::from(git_text(&legacy_path, &["rev-parse", "--absolute-git-dir"]).trim());
    fs::create_dir_all(legacy_path.join("leap")).unwrap();
    fs::write(admin.join("leap-environment-id"), "legacy-id\n").unwrap();
    manager
        .with_metadata(|metadata| {
            metadata.migration_complete = false;
            Ok(())
        })
        .unwrap();

    manager.migrate_legacy_locked().unwrap();
    let loc = locator(7);
    let spec = CheckoutSpec::new(&loc, &head).unwrap();
    let saved = manager
        .read_records()
        .unwrap()
        .records
        .into_iter()
        .find(|record| record.key == spec.key())
        .unwrap();
    assert_eq!(saved.kind, RecordKind::Legacy);
    assert_eq!(saved.path, legacy_path.display().to_string());
    assert!(saved.pending_cleanup);
    assert!(!saved.storage_teardown_complete);
    assert_eq!(
        saved.git_admin_dir.as_deref(),
        Some(admin.to_str().unwrap())
    );

    let error = manager
        .acquire(&loc, &head)
        .err()
        .expect("acquire unexpectedly succeeded");
    assert!(error
        .to_string()
        .contains("legacy review checkout still requires migration"));
    let unchanged = manager
        .read_records()
        .unwrap()
        .records
        .into_iter()
        .find(|record| record.key == spec.key())
        .unwrap();
    assert_eq!(unchanged.kind, RecordKind::Legacy);
    assert_eq!(unchanged.path, legacy_path.display().to_string());
    assert!(unchanged.pending_cleanup);
    assert!(!unchanged.storage_teardown_complete);
    assert!(legacy_path.exists());
    assert!(admin.join("leap-environment-id").exists());

    fs::remove_file(admin.join("leap-environment-id")).unwrap();
    manager.migrate_legacy_locked().unwrap();
    assert!(!legacy_path.exists());
    assert!(manager
        .read_records()
        .unwrap()
        .records
        .into_iter()
        .all(|record| record.key != spec.key()));

    let lease = manager.acquire(&loc, &head).unwrap();
    let linked = manager
        .read_records()
        .unwrap()
        .records
        .into_iter()
        .find(|record| record.key == spec.key())
        .unwrap();
    assert_eq!(linked.kind, RecordKind::Linked);
    assert_eq!(
        linked.path,
        manager
            .worktrees_root()
            .join(spec.key())
            .display()
            .to_string()
    );
    assert!(!linked.materialized);
    assert!(lease.teardown().unwrap());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn destroy_busy_cached_checkout_marks_pending_for_startup_retry() {
    let root = temp_root("destroy-busy");
    let (remote, _seed, head) = bare_remote(&root, None);
    set_pull_head(&remote, 13, &head);
    let cache = root.join("cache");
    let manager = ReviewCheckoutManager::at_root(cache.clone(), Some(remote.display().to_string()));
    let lease = manager.acquire(&locator(13), &head).unwrap();
    let path = manager.materialize(&lease).unwrap();

    assert!(manager.destroy_cached_path(&path).is_err());
    assert!(path.exists());
    assert!(manager
        .read_records()
        .unwrap()
        .records
        .iter()
        .any(|record| record.path == path.display().to_string() && record.pending_cleanup));

    drop(lease);
    ReviewCheckoutManager::at_root(cache, Some(remote.display().to_string()))
        .startup()
        .unwrap();
    assert!(!path.exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn destroy_cached_checkout_marks_pending_when_repository_is_busy() {
    let root = temp_root("destroy-repo-busy");
    let (remote, _seed, head) = bare_remote(&root, None);
    set_pull_head(&remote, 14, &head);
    let cache = root.join("cache");
    let manager = ReviewCheckoutManager::at_root(cache.clone(), Some(remote.display().to_string()));
    let lease = manager.acquire(&locator(14), &head).unwrap();
    let path = manager.materialize(&lease).unwrap();
    drop(lease);

    let spec = CheckoutSpec::new(&locator(14), &head).unwrap();
    let held = manager.repo_lock(&spec).unwrap();
    assert!(manager.destroy_cached_path(&path).is_err());
    assert!(path.exists());
    assert!(manager
        .read_records()
        .unwrap()
        .records
        .iter()
        .any(|record| record.path == path.display().to_string() && record.pending_cleanup));

    drop(held);
    ReviewCheckoutManager::at_root(cache, Some(remote.display().to_string()))
        .startup()
        .unwrap();
    assert!(!path.exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn retention_skips_active_lease_and_retries_old_pending_failure() {
    let root = temp_root("retention");
    let (retention_remote, _retention_seed, retention_head) = bare_remote(&root, None);
    let cache = root.join("cache");
    let manager = ReviewCheckoutManager::at_root(cache.clone(), None);
    let spec = CheckoutSpec::new(&locator(1), &retention_head).unwrap();
    let lease = manager.acquire(&locator(1), &spec.head_oid).unwrap();
    let path = manager.worktrees_root().join(spec.key());
    clone_legacy(
        &retention_remote,
        &path,
        "https://github.com/acme/thing.git",
    );
    manager
        .with_metadata(|metadata| {
            let current = metadata
                .records
                .iter_mut()
                .find(|item| item.key == spec.key())
                .unwrap();
            current.path = path.display().to_string();
            current.last_used_secs = 0;
            current.materialized = true;
            current.kind = RecordKind::Legacy;
            Ok(())
        })
        .unwrap();

    ReviewCheckoutManager::at_root(cache.clone(), None)
        .startup()
        .unwrap();
    assert!(path.exists());
    assert!(manager
        .read_records()
        .unwrap()
        .records
        .iter()
        .any(|item| item.key == spec.key()));
    drop(lease);
    ReviewCheckoutManager::at_root(cache.clone(), None)
        .startup()
        .unwrap();
    assert!(!path.exists());
    assert!(!manager
        .read_records()
        .unwrap()
        .records
        .iter()
        .any(|item| item.key == spec.key()));

    let remote = retention_remote;
    let head = retention_head;
    let failed_manager = ReviewCheckoutManager::at_root(
        root.join("failed-cache"),
        Some(remote.display().to_string()),
    );
    let failed_path = failed_manager
        .worktrees_root()
        .join(format!("acme__thing__pr2__{head}"));
    clone_legacy(&remote, &failed_path, "https://github.com/acme/thing.git");
    fs::create_dir_all(failed_path.join("leap")).unwrap();
    fs::write(
        failed_path.join("leap/package.json"),
        r#"{"name":"minimal"}"#,
    )
    .unwrap();
    let admin = PathBuf::from(git_text(&failed_path, &["rev-parse", "--absolute-git-dir"]).trim());
    fs::write(admin.join("leap-environment-id"), "old-id\n").unwrap();
    let failed_spec = CheckoutSpec {
        owner: "acme".into(),
        repo: "thing".into(),
        number: 2,
        head_oid: head.clone(),
    };
    let mut failed_record = record(&failed_spec, &failed_path, RecordKind::Legacy);
    failed_record.git_admin_dir = Some(admin.display().to_string());
    failed_record.pending_cleanup = true;
    failed_record.last_used_secs = now_secs();
    write_record(&failed_manager, failed_record, true);

    failed_manager.startup().unwrap();
    assert!(failed_path.exists());
    assert!(failed_path.join("leap/package.json").exists());
    assert!(failed_path.join(".git/leap-environment-id").exists());
    assert!(failed_manager
        .read_records()
        .unwrap()
        .records
        .iter()
        .any(|item| item.key == failed_spec.key() && item.pending_cleanup));

    fs::remove_file(failed_path.join("leap/package.json")).unwrap();
    ReviewCheckoutManager::at_root(root.join("failed-cache"), None)
        .startup()
        .unwrap();
    assert!(failed_path.exists());
    assert!(failed_path.join(".git/leap-environment-id").exists());
    assert!(failed_manager
        .read_records()
        .unwrap()
        .records
        .iter()
        .any(|item| item.key == failed_spec.key() && item.pending_cleanup));

    fs::remove_file(failed_path.join(".git/leap-environment-id")).unwrap();
    ReviewCheckoutManager::at_root(root.join("failed-cache"), None)
        .startup()
        .unwrap();
    assert!(!failed_path.exists());
    assert!(!failed_manager
        .read_records()
        .unwrap()
        .records
        .iter()
        .any(|item| item.key == failed_spec.key()));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn legacy_migration_removes_clones_and_no_id_leap_checkout_idempotently() {
    let root = temp_root("migration");
    let (remote, _seed, head) = bare_remote(&root, None);
    let cache = root.join("cache");
    let manager = ReviewCheckoutManager::at_root(cache, None);
    fs::create_dir_all(manager.worktrees_root()).unwrap();
    let origin = "https://github.com/acme/thing.git";
    let clone_path = manager
        .worktrees_root()
        .join(format!("Acme__Thing__pr9__{}", &head[..12]));
    clone_legacy(&remote, &clone_path, origin);
    let package_path = manager
        .worktrees_root()
        .join(format!("acme__thing__pr10__{head}.tmp-clear-123-1"));
    clone_legacy(&remote, &package_path, origin);
    fs::create_dir_all(package_path.join("leap")).unwrap();
    fs::write(
        package_path.join("leap/package.json"),
        r#"{"name":"no-deps"}"#,
    )
    .unwrap();

    manager.startup().unwrap();
    assert!(!clone_path.exists());
    assert!(!package_path.exists());
    assert!(manager.read_records().unwrap().records.is_empty());
    ReviewCheckoutManager::at_root(root.join("cache"), None)
        .startup()
        .unwrap();
    assert!(manager
        .worktrees_root()
        .read_dir()
        .unwrap()
        .next()
        .is_none());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn failed_vp_destroy_preserves_checkout_admin_and_git_registration() {
    let root = temp_root("vp-failure");
    let (remote, _seed, head) = bare_remote(&root, Some(r#"{"name":"minimal"}"#));
    set_pull_head(&remote, 11, &head);
    let manager =
        ReviewCheckoutManager::at_root(root.join("cache"), Some(remote.display().to_string()));
    let lease = manager.acquire(&locator(11), &head).unwrap();
    let path = manager.materialize(&lease).unwrap();
    let base = manager.repos_root().join("acme__thing");
    let record = manager
        .read_records()
        .unwrap()
        .records
        .into_iter()
        .find(|item| item.key == CheckoutSpec::new(&locator(11), &head).unwrap().key())
        .unwrap();
    let admin = PathBuf::from(record.git_admin_dir.unwrap());
    fs::write(admin.join("leap-environment-id"), "vp-failure-id\n").unwrap();

    let error = lease.teardown().unwrap_err();
    assert!(error.to_string().contains("vp run storage:destroy"));
    assert!(path.exists());
    assert!(admin.join("leap-environment-id").is_file());
    assert!(registered_path(&base, &path).unwrap());
    assert!(manager
        .read_records()
        .unwrap()
        .records
        .iter()
        .any(|item| item.key == record.key));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn checkpoints_prune_safe_missing_worktree_and_finish_partial_legacy_removal() {
    let root = temp_root("checkpoints");
    let (remote, _seed, head) = bare_remote(&root, None);
    set_pull_head(&remote, 12, &head);
    let cache = root.join("checkpoint-cache");
    let manager = ReviewCheckoutManager::at_root(cache.clone(), Some(remote.display().to_string()));
    let lease = manager.acquire(&locator(12), &head).unwrap();
    let path = manager.materialize(&lease).unwrap();
    let key = CheckoutSpec::new(&locator(12), &head).unwrap().key();
    drop(lease);
    fs::remove_dir_all(&path).unwrap();
    manager
        .with_metadata(|metadata| {
            let current = metadata
                .records
                .iter_mut()
                .find(|item| item.key == key)
                .unwrap();
            current.pending_cleanup = true;
            current.storage_teardown_complete = true;
            current.last_used_secs = 0;
            Ok(())
        })
        .unwrap();
    ReviewCheckoutManager::at_root(cache.clone(), Some(remote.display().to_string()))
        .startup()
        .unwrap();
    assert!(!manager
        .read_records()
        .unwrap()
        .records
        .iter()
        .any(|item| item.key == key));
    assert!(!registered_path(&manager.repos_root().join("acme__thing"), &path).unwrap());

    let partial_cache = root.join("partial-cache");
    let partial = ReviewCheckoutManager::at_root(partial_cache, None);
    fs::create_dir_all(partial.worktrees_root()).unwrap();
    let partial_spec = CheckoutSpec {
        owner: "acme".into(),
        repo: "thing".into(),
        number: 13,
        head_oid: head,
    };
    let partial_path = partial.worktrees_root().join(partial_spec.key());
    clone_legacy(&remote, &partial_path, "https://github.com/acme/thing.git");
    let mut partial_record = record(&partial_spec, &partial_path, RecordKind::Legacy);
    partial_record.pending_cleanup = true;
    partial_record.storage_teardown_complete = true;
    write_record(&partial, partial_record, true);
    partial.startup().unwrap();
    assert!(!partial_path.exists());
    assert!(!partial
        .read_records()
        .unwrap()
        .records
        .iter()
        .any(|item| item.key == partial_spec.key()));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn missing_unproven_and_external_or_symlink_paths_keep_identity_and_directories() {
    let root = temp_root("guards");
    let cache = root.join("cache");
    let manager = ReviewCheckoutManager::at_root(cache.clone(), None);
    fs::create_dir_all(manager.worktrees_root()).unwrap();

    let missing_spec = CheckoutSpec {
        owner: "acme".into(),
        repo: "thing".into(),
        number: 20,
        head_oid: "b".repeat(40),
    };
    let missing_path = manager.worktrees_root().join(missing_spec.key());
    let mut missing = record(&missing_spec, &missing_path, RecordKind::Linked);
    missing.git_admin_dir = Some("/managed/checkpoint/admin".into());
    write_record(&manager, missing, true);

    let external = root.join("external");
    fs::create_dir_all(&external).unwrap();
    fs::write(external.join("keep"), "keep\n").unwrap();
    let external_spec = CheckoutSpec {
        owner: "acme".into(),
        repo: "thing".into(),
        number: 21,
        head_oid: "c".repeat(40),
    };
    let mut external_record = record(&external_spec, &external, RecordKind::Legacy);
    external_record.last_used_secs = 0;
    write_record(&manager, external_record, true);

    let symlink_target = root.join("symlink-target");
    fs::create_dir_all(&symlink_target).unwrap();
    fs::write(symlink_target.join("keep"), "keep\n").unwrap();
    let symlink_spec = CheckoutSpec {
        owner: "acme".into(),
        repo: "thing".into(),
        number: 22,
        head_oid: "d".repeat(40),
    };
    let symlink_path = manager.worktrees_root().join(symlink_spec.key());
    std::os::unix::fs::symlink(&symlink_target, &symlink_path).unwrap();
    let mut symlink_record = record(&symlink_spec, &symlink_path, RecordKind::Legacy);
    symlink_record.last_used_secs = 0;
    write_record(&manager, symlink_record, true);

    manager.startup().unwrap();
    assert!(external.join("keep").is_file());
    assert!(symlink_target.join("keep").is_file());
    assert!(fs::symlink_metadata(&symlink_path)
        .unwrap()
        .file_type()
        .is_symlink());
    let records = manager.read_records().unwrap().records;
    assert!(records.iter().any(|item| item.key == missing_spec.key()));
    assert!(records.iter().any(|item| item.key == external_spec.key()));
    assert!(records.iter().any(|item| item.key == symlink_spec.key()));
    assert!(records.iter().all(|item| item.pending_cleanup));
    let _ = fs::remove_dir_all(root);
}
