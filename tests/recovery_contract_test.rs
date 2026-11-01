//! Cross-process recovery contracts. Linux config isolation uses XDG_CONFIG_HOME;
//! Windows known folders require a separate profile and are not redirected here.
#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fixture {
    root: tempfile::TempDir,
    store: PathBuf,
}

impl Fixture {
    fn new(mode: &str, extra: &str) -> Self {
        let root = common::tempdir();
        let config = root.path().join("config/rtk");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.toml"),
            format!("[retriever]\nmode = {mode:?}\n{extra}\n"),
        )
        .unwrap();
        Self {
            store: root.path().join("store"),
            root,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = common::rtk_command();
        cmd.env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("RTK_RECALL_DB", self.store.join("recall.db"))
            .env("RTK_TEE_DIR", self.store.join("tee"))
            .env_remove("RTK_RECALL")
            .env_remove("RTK_TEE")
            .current_dir(self.root.path());
        cmd
    }

    fn failure(&self, family: &str, sentinel: &str) -> Output {
        let payload = format!(
            "{}\nERROR {sentinel}\n",
            "fabricated diagnostic\n".repeat(100)
        );
        fs::write(self.root.path().join("payload"), payload).unwrap();
        let script = self.root.path().join("failure");
        fs::write(&script, "#!/bin/sh\ncat payload >&2\nexit 23\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let mut cmd = self.command();
        if family == "ruff" {
            fs::copy(&script, self.root.path().join("ruff")).unwrap();
            cmd.env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.path().display()),
            )
            .args(["ruff", "check"]);
        } else {
            cmd.args([family, "/bin/sh"]).arg(script);
        }
        let out = bounded(cmd);
        assert_eq!(out.status.code(), Some(23), "{}", combined(&out));
        assert!(combined(&out).contains(sentinel), "{}", combined(&out));
        out
    }

    fn recover(&self, out: &Output) -> String {
        let text = combined(out);
        let hint = text
            .split("[full output: ")
            .nth(1)
            .expect("full recovery hint")
            .split(']')
            .next()
            .unwrap();
        if let Some(hash) = hint.strip_prefix("rtk recall ") {
            let mut cmd = self.command();
            cmd.args(["recall", hash, "--full"]);
            let recalled = bounded(cmd);
            assert!(recalled.status.success(), "{}", combined(&recalled));
            String::from_utf8(recalled.stdout).unwrap()
        } else {
            fs::read_to_string(hint.trim_matches('"')).expect("hint names a retained file")
        }
    }
}

fn bounded(mut cmd: Command) -> Output {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let out = child.wait_with_output().unwrap();
            panic!("CLI timed out: {}", combined(&out));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn no_hint(out: &Output) {
    let text = combined(out);
    assert!(
        !text.contains("full output:") && !text.contains("partial output"),
        "{text}"
    );
}

#[test]
fn failures_keep_exit_diagnostics_and_recoverable_output_across_command_families() {
    for mode in ["tee", "sqlite"] {
        for family in ["err", "test", "ruff"] {
            let fixture = Fixture::new(mode, "");
            let out = fixture.failure(family, "RECOVERY_WRITABLE");
            let payload = fs::read_to_string(fixture.root.path().join("payload")).unwrap();
            if family == "ruff" {
                // A failing stdout-only filter forwards all stderr verbatim;
                // it has no hidden bytes that need a recovery hint.
                assert!(out.stdout.is_empty());
                assert_eq!(String::from_utf8(out.stderr).unwrap(), payload);
            } else {
                assert_eq!(fixture.recover(&out), payload);
            }
        }
    }
}

#[test]
fn unavailable_storage_preserves_failures_without_a_hint() {
    for mode in ["tee", "sqlite"] {
        for family in ["err", "test", "ruff"] {
            let fixture = Fixture::new(mode, "");
            fs::write(&fixture.store, "fabricated non-directory").unwrap();
            let out = fixture.failure(family, "RECOVERY_BLOCKED");
            no_hint(&out);
            if family == "ruff" {
                assert!(out.stdout.is_empty());
                assert!(
                    String::from_utf8_lossy(&out.stderr).contains(
                        &fs::read_to_string(fixture.root.path().join("payload")).unwrap()
                    )
                );
            }
        }
    }
}

#[test]
fn rotation_never_advertises_a_deleted_file() {
    let fixture = Fixture::new("tee", "tee_max_files = 1");
    fs::create_dir_all(fixture.store.join("tee")).unwrap();
    fs::write(
        fixture.store.join("tee/9999999999_z.log"),
        "older fabricated run",
    )
    .unwrap();
    let out = fixture.failure("err", "ROTATION_CURRENT");
    assert!(fixture.recover(&out).contains("ROTATION_CURRENT"));
    assert_eq!(fs::read_dir(fixture.store.join("tee")).unwrap().count(), 1);
}

fn hash(out: &Output) -> String {
    combined(out)
        .split("rtk recall ")
        .nth(1)
        .expect("recall hint")
        .split(']')
        .next()
        .unwrap()
        .to_owned()
}

fn assert_evicted(fixture: &Fixture, key: &str) {
    let mut cmd = fixture.command();
    cmd.args(["recall", key, "--full"]);
    let out = bounded(cmd);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        combined(&out).contains("no matching entry"),
        "{}",
        combined(&out)
    );
}

#[test]
fn sqlite_fifo_and_write_triggered_expiry_cross_processes() {
    let fifo = Fixture::new("sqlite", "max_entries = 2\nretention_days = 0");
    let first = fifo.failure("err", "FIFO_FIRST");
    let second = fifo.failure("err", "FIFO_SECOND");
    let third = fifo.failure("err", "FIFO_THIRD");
    assert_evicted(&fifo, &hash(&first));
    assert!(fifo.recover(&second).contains("FIFO_SECOND"));
    assert!(fifo.recover(&third).contains("FIFO_THIRD"));

    let age = Fixture::new("sqlite", "retention_days = 1");
    let old = age.failure("err", "AGE_OLD");
    let conn = rusqlite::Connection::open(age.store.join("recall.db")).unwrap();
    conn.execute(
        "UPDATE recall SET created_at = 0 WHERE hash = ?1",
        [&hash(&old)],
    )
    .unwrap();
    drop(conn);
    let new = age.failure("err", "AGE_NEW");
    assert_evicted(&age, &hash(&old));
    assert!(age.recover(&new).contains("AGE_NEW"));
}

struct RestorePermissions(PathBuf);
impl Drop for RestorePermissions {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
    }
}

#[test]
fn permission_denied_ancestor_does_not_hide_the_failure() {
    for mode in ["tee", "sqlite"] {
        let mut fixture = Fixture::new(mode, "");
        let denied = fixture.root.path().join("denied");
        fs::create_dir(&denied).unwrap();
        let _restore = RestorePermissions(denied.clone());
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o500)).unwrap();
        let probe = fs::create_dir(denied.join("probe"));
        assert_eq!(
            probe.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied,
            "requires a user without permission bypass"
        );
        fixture.store = denied.join("missing/store");
        no_hint(&fixture.failure("err", "PERMISSION_SENTINEL"));
    }
}

/// Explicit prerequisite: Linux user/mount namespaces, mount and timeout tools.
/// The mount namespace vanishes with its bounded child; no host mount is changed.
#[test]
#[ignore = "requires unprivileged user/mount namespaces and tmpfs mounts"]
fn full_filesystem_preserves_all_failure_families() {
    let fixture = Fixture::new("sqlite", "");
    let full = fixture.root.path().join("full");
    fs::create_dir(&full).unwrap();
    let mut cmd = common::native_command("timeout");
    cmd.args(["--kill-after=5", "60", "unshare", "--user", "--map-root-user", "--mount", "/bin/sh", "-eu", "-c"])
        .arg("mount --make-rprivate /; mount -t tmpfs -o size=1m,nr_inodes=128 tmpfs \"$1\"; exec \"$2\" --exact full_filesystem_inner --ignored --nocapture")
        .arg("recovery-tmpfs").arg(&full).arg(std::env::current_exe().unwrap())
        .env("RECOVERY_CONTRACT_FULL_FS", &full);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", combined(&out));
}

#[test]
#[ignore = "called only by the namespaced full-filesystem test"]
fn full_filesystem_inner() {
    use std::io::Write;
    let full = PathBuf::from(
        std::env::var_os("RECOVERY_CONTRACT_FULL_FS").expect("namespaced caller required"),
    );
    let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap();
    let mount = mounts
        .lines()
        .find(|line| line.split_whitespace().nth(4) == full.to_str())
        .expect("dedicated tmpfs mount required");
    assert!(
        mount.contains(" - tmpfs ") && mount.contains("size=1024k"),
        "requires the caller's 1 MiB tmpfs: {mount}"
    );
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(full.join("filler"))
        .unwrap();
    let mut observed_full = false;
    for _ in 0..32 {
        if let Err(error) = file.write_all(&[b'x'; 65536]) {
            assert_eq!(error.raw_os_error(), Some(28));
            observed_full = true;
            break;
        }
    }
    assert!(
        observed_full,
        "expected ENOSPC within the 2 MiB write ceiling"
    );
    assert_eq!(
        fs::write(full.join("probe"), b"x")
            .unwrap_err()
            .raw_os_error(),
        Some(28)
    );
    for mode in ["tee", "sqlite"] {
        for family in ["err", "test", "ruff"] {
            let mut fixture = Fixture::new(mode, "");
            fixture.store = full.join(format!("{mode}-{family}"));
            no_hint(&fixture.failure(family, "ENOSPC_SENTINEL"));
        }
    }
}
