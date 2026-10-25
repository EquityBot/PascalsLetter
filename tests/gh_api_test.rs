//! `rtk gh api` drops api.github.com `*_url` links from a JSON response and stores the whole
//! response for `rtk recall`. Everything it does not filter must reach the caller byte for
//! byte: invalid UTF-8, CRLF, a missing final newline, a response past the 10 MiB line
//! capture of the streaming runner, and any response of a failed call.

use std::fs;
use std::path::Path;
use std::process::Output;

mod common;

/// A `gh` in `dir` that prints `payload` verbatim and exits `code`.
fn fake_gh(dir: &Path, payload: &[u8], code: i32) {
    let body = dir.join("body.bin");
    fs::write(&body, payload).expect("write payload");
    #[cfg(windows)]
    fs::write(
        dir.join("gh.cmd"),
        format!("@type \"{}\"\r\n@exit /b {code}\r\n", body.display()),
    )
    .expect("write fake gh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("gh");
        fs::write(
            &path,
            format!("#!/bin/sh\ncat '{}'\nexit {code}\n", body.display()),
        )
        .expect("write fake gh");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake gh");
    }
}

fn rtk_with(dir: &Path, args: &[&str]) -> Output {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let path = format!(
        "{}{sep}{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    common::rtk_command()
        .args(args)
        .env("PATH", path)
        .output()
        .expect("run rtk")
}

const LINKED: &[u8] = br#"{"id":1,"url":"https://api.github.com/repos/o/r","forks_url":"https://api.github.com/repos/o/r/forks"}"#;

#[test]
fn links_are_dropped_and_the_whole_response_is_recallable() {
    let dir = tempfile::tempdir().expect("tempdir");
    fake_gh(dir.path(), LINKED, 0);

    let out = rtk_with(dir.path(), &["gh", "api", "repos/o/r"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(!stdout.contains("forks_url"), "link kept: {stdout}");
    assert!(stdout.contains(r#""id":1"#), "value lost: {stdout}");
    let hash = stderr
        .split("rtk recall ")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap_or_else(|| panic!("no recall hint: {stderr}"));
    let full = rtk_with(dir.path(), &["recall", hash, "--full"]);
    assert!(
        String::from_utf8_lossy(&full.stdout).contains("forks_url"),
        "recall lost the dropped link"
    );
}

fn assert_verbatim(payload: &[u8], code: i32) {
    let dir = tempfile::tempdir().expect("tempdir");
    fake_gh(dir.path(), payload, code);

    let out = rtk_with(dir.path(), &["gh", "api", "repos/o/r"]);

    assert_eq!(out.status.code(), Some(code));
    assert!(
        out.stdout == payload,
        "stdout changed: {} bytes in, {} out",
        payload.len(),
        out.stdout.len()
    );
}

#[test]
fn bytes_that_are_not_json_pass_through_unchanged() {
    // Not at the start: cmd's `type` would read FF FE there as a UTF-16 byte-order mark.
    assert_verbatim(b"raw \xff\xfe\r\nline two, no final newline", 0);
}

#[test]
fn a_failed_call_passes_through_unchanged() {
    assert_verbatim(LINKED, 1);
}

#[test]
fn a_response_past_the_capture_limit_passes_through_unchanged() {
    // One 11 MiB line: the streaming runner drops a line that would cross its 10 MiB cap.
    let mut big = br#"{"blob":""#.to_vec();
    big.extend(std::iter::repeat_n(b'x', 11 * 1024 * 1024));
    big.extend(br#""}"#);
    assert_verbatim(&big, 0);
}

#[test]
fn links_stay_when_the_response_cannot_be_stored() {
    let dir = tempfile::tempdir().expect("tempdir");
    fake_gh(dir.path(), LINKED, 0);
    let sep = if cfg!(windows) { ";" } else { ":" };
    let path = format!(
        "{}{sep}{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let out = common::rtk_command()
        .args(["gh", "api", "repos/o/r"])
        .env("PATH", path)
        .env("RTK_RECALL", "0")
        .output()
        .expect("run rtk");

    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, LINKED, "filtered without a stored original");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("dropped"));
}
