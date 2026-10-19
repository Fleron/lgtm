use anyhow::{bail, Context, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::review_checkouts::ReviewCheckoutManager;

#[derive(Debug, Eq, PartialEq)]
struct Snapshot {
    containers: Vec<String>,
    volumes: Vec<String>,
}

struct StorageCleanup {
    checkout: PathBuf,
    admin: PathBuf,
    active: bool,
}

impl Drop for StorageCleanup {
    fn drop(&mut self) {
        if !self.active || !self.admin.join("leap-environment-id").is_file() {
            return;
        }
        let leap = self.checkout.join("leap");
        if !leap.is_dir() {
            eprintln!(
                "live checkout cleanup unavailable: {} is gone",
                leap.display()
            );
            return;
        }
        if let Err(err) = run("vp", &["run", "storage:destroy"], Some(&leap)) {
            eprintln!("live checkout storage cleanup failed: {err:#}");
        }
    }
}

fn run(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut command = Command::new(program);
    if program == "docker" {
        command.args(["--context", "colima"]);
    }
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .with_context(|| format!("running {program}"))?;
    if program == "vp" && args == ["run", "storage:up"] {
        eprintln!(
            "vp run storage:up result: status={:?}; stdout={:?}; stderr={:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn copy_node_modules(source: &Path, destination: &Path) -> Result<()> {
    let copy_on_write = Command::new("cp")
        .arg("-cR")
        .arg(source)
        .arg(destination)
        .output()
        .context("copying node_modules with APFS copy-on-write")?;
    if copy_on_write.status.success() {
        return Ok(());
    }
    eprintln!(
        "cp -cR unavailable; falling back to cp -R: status={:?}; stderr={:?}",
        copy_on_write.status.code(),
        String::from_utf8_lossy(&copy_on_write.stderr)
    );
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if metadata.file_type().is_dir() {
            fs::remove_dir_all(destination)?;
        } else {
            fs::remove_file(destination)?;
        }
    }
    let plain = Command::new("cp")
        .arg("-R")
        .arg(source)
        .arg(destination)
        .output()
        .context("copying node_modules")?;
    if !plain.status.success() {
        bail!(
            "cp -R failed: {}",
            String::from_utf8_lossy(&plain.stderr).trim()
        );
    }
    Ok(())
}

fn docker_lines(args: &[&str]) -> Result<Vec<String>> {
    let mut lines = run("docker", args, None)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.sort_unstable();
    Ok(lines)
}

fn snapshot() -> Result<Snapshot> {
    Ok(Snapshot {
        containers: docker_lines(&[
            "ps",
            "-a",
            "--filter",
            "label=dev.leap.managed=worktree-v2",
            "--format",
            "{{.ID}}",
        ])?,
        volumes: docker_lines(&["volume", "ls", "--format", "{{.Name}}"])?,
    })
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str().context("test path is not UTF-8")
}

fn leap_environment_id(path: &Path) -> Result<String> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("decoding {}", path.display()))
}

fn leap_ids(admin: &Path) -> Result<Vec<String>> {
    let mut files = vec![admin.join("leap-environment-id")];
    if let Ok(entries) = fs::read_dir(admin.join("worktrees")) {
        files.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path().join("leap-environment-id")),
        );
    }
    Ok(files
        .into_iter()
        .filter_map(|path| leap_environment_id(&path).ok())
        .filter(|id| !id.is_empty())
        .collect())
}

#[test]
#[ignore = "requires LGTM_TEST_LEAP_ROOT, LGTM_TEST_FIXTURE_ROOT, vp, and Colima"]
fn real_checkout_storage_teardown_matches_colima_baseline() -> Result<()> {
    let source = PathBuf::from(
        env::var_os("LGTM_TEST_LEAP_ROOT").context("set LGTM_TEST_LEAP_ROOT for this test")?,
    );
    let fixture = PathBuf::from(
        env::var_os("LGTM_TEST_FIXTURE_ROOT")
            .context("set LGTM_TEST_FIXTURE_ROOT to an owned temporary directory for this test")?,
    );
    let source_text = path_text(&source)?;
    let source_head = run("git", &["-C", source_text, "rev-parse", "HEAD"], None)?;
    if source_head.len() != 40 {
        bail!("source HEAD is not a full commit id: {source_head}");
    }
    let source_admin = PathBuf::from(run(
        "git",
        &["-C", source_text, "rev-parse", "--absolute-git-dir"],
        None,
    )?);
    let existing_ids = leap_ids(&source_admin)?;
    let package = source.join("leap/package.json");
    let source_deps = source.join("leap/node_modules");
    if !package.is_file() || !source_deps.is_dir() {
        bail!("source must contain leap/package.json and working leap/node_modules");
    }
    let context = run("docker", &["context", "show"], None)?;
    if context != "colima" {
        bail!("Colima context is unavailable (reported {context:?})");
    }
    let baseline = snapshot()?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    fs::create_dir_all(&fixture)?;
    let run_root = fixture.join(format!("run-{}-{nonce}", std::process::id()));
    fs::create_dir(&run_root)?;
    eprintln!("live checkout fixture: {}", run_root.display());
    eprintln!("baseline: {baseline:?}");

    let remote = run_root.join("remote.git");
    let remote_text = path_text(&remote)?;
    run(
        "git",
        &["clone", "--bare", "--shared", source_text, remote_text],
        None,
    )?;
    run(
        "git",
        &[
            "--git-dir",
            remote_text,
            "update-ref",
            "refs/pull/1/head",
            &source_head,
        ],
        None,
    )?;
    let cache = run_root.join("cache");
    let manager = ReviewCheckoutManager::at_root(cache.clone(), Some(remote_text.to_owned()));
    let locator = gh::PrLocator {
        owner: "ownertest".to_owned(),
        repo: "repo".to_owned(),
        number: 1,
    };
    let lease = manager.acquire(&locator, &source_head)?;
    let checkout = manager.materialize(&lease)?;
    let checkout_text = path_text(&checkout)?;
    let admin = PathBuf::from(run(
        "git",
        &["-C", checkout_text, "rev-parse", "--absolute-git-dir"],
        None,
    )?);
    let deps = checkout.join("leap/node_modules");
    if fs::symlink_metadata(&deps).is_ok() {
        fs::remove_file(&deps).context("removing checkout node_modules link")?;
    }
    copy_node_modules(&source_deps, &deps)?;
    let mut cleanup = StorageCleanup {
        checkout: checkout.clone(),
        admin: admin.clone(),
        active: true,
    };
    run("vp", &["run", "storage:up"], Some(&checkout.join("leap")))?;
    let id = leap_environment_id(&admin.join("leap-environment-id"))?;
    assert!(!id.is_empty(), "missing generated Git-admin Leap ID");
    assert!(
        !existing_ids.contains(&id),
        "Git-admin Leap ID was reused: {id}"
    );
    let worktree_filter = format!("label=dev.leap.worktree={id}");
    let owned_services = run(
        "docker",
        &[
            "ps",
            "-a",
            "--filter",
            "label=dev.leap.managed=worktree-v2",
            "--filter",
            &worktree_filter,
            "--format",
            "{{.Label \"com.docker.compose.service\"}}\t{{.ID}}",
        ],
        None,
    )?;
    let project_filter = format!("label=com.docker.compose.project=leap-{id}");
    let project_services = run(
        "docker",
        &[
            "ps",
            "-a",
            "--filter",
            &project_filter,
            "--format",
            "{{.Label \"com.docker.compose.service\"}}\t{{.ID}}\t{{.Label \"dev.leap.managed\"}}\t{{.Label \"dev.leap.worktree\"}}\t{{.Label \"dev.leap.repository\"}}",
        ],
        None,
    )?;
    eprintln!("project-scoped service rows for {id}: {project_services:?}");
    let service_names = owned_services
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect::<Vec<_>>();
    eprintln!("owned service rows for {id}: {owned_services:?}; names: {service_names:?}");
    assert!(service_names.contains(&"db"));
    assert!(service_names.contains(&"blob"));
    let owned_volumes = docker_lines(&[
        "volume",
        "ls",
        "--filter",
        "label=dev.leap.managed=worktree-v2",
        "--filter",
        &worktree_filter,
        "--format",
        "{{.Name}}",
    ])?;
    assert!(owned_volumes.iter().any(|name| name.ends_with("_db-data")));
    assert!(owned_volumes
        .iter()
        .any(|name| name.ends_with("_blob-data")));
    eprintln!(
        "provisioned: {:?}; services: {owned_services:?}; volumes: {owned_volumes:?}",
        snapshot()?
    );

    assert!(lease.teardown()?);
    assert!(!checkout.exists(), "checkout survived teardown");
    assert!(!admin.exists(), "Git admin directory survived teardown");
    let base = cache.join("repos/ownertest__repo");
    let base_text = path_text(&base)?;
    let registrations = run(
        "git",
        &["-C", base_text, "worktree", "list", "--porcelain"],
        None,
    )?;
    assert!(!registrations
        .lines()
        .any(|line| line == format!("worktree {checkout_text}")));
    let after = snapshot()?;
    eprintln!("after teardown: {after:?}");
    assert_eq!(after, baseline, "Colima resources changed after teardown");
    assert_eq!(
        run("git", &["-C", source_text, "rev-parse", "HEAD"], None)?,
        source_head
    );
    assert!(package.is_file() && source_deps.is_dir());
    cleanup.active = false;
    Ok(())
}
