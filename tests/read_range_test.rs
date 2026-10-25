use std::fs;
use std::io::Write;
use std::process::{Output, Stdio};

mod common;

fn from_stdin(input: &[u8], args: &[&str]) -> Output {
    let mut child = common::rtk_command()
        .args(["read", "-"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run range read");
    child
        .stdin
        .take()
        .expect("prepare or execute range fixture")
        .write_all(input)
        .expect("prepare or execute range fixture");
    child
        .wait_with_output()
        .expect("prepare or execute range fixture")
}

#[test]
fn ranges_preserve_selected_bytes_for_files_and_stdin() {
    let dir = common::tempdir();
    let file = dir.path().join("range.log");
    for input in [
        b"first\n\nthird\nlast".as_slice(),
        b"first\r\n\r\nthird\r\nlast\r\n".as_slice(),
        b"\xff\n\0\xfe\r\nlast".as_slice(),
        "first\nnaïve 🐔\nlast".as_bytes(),
        b"".as_slice(),
    ] {
        fs::write(&file, input).expect("prepare or execute range fixture");
        let lines: Vec<&[u8]> = input.split_inclusive(|&byte| byte == b'\n').collect();
        for (first, count) in [(1, 0), (1, 1), (2, 1), (2, 3), (3, 99), (99, 1)] {
            let expected = lines
                .iter()
                .skip(first - 1)
                .take(count)
                .copied()
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            let first = first.to_string();
            let count = count.to_string();
            let args = ["--from", &first, "--lines", &count];
            let from_file = common::rtk_command()
                .arg("read")
                .arg(&file)
                .args(args)
                .output()
                .expect("prepare or execute range fixture");
            for output in [from_file, from_stdin(input, &args)] {
                assert!(output.status.success(), "{args:?}: {:?}", output.stderr);
                assert_eq!(output.stdout, expected, "{args:?}");
            }
        }
    }
}

#[test]
fn ranges_default_to_the_first_line_and_number_original_lines() {
    let input = b"one\r\ntwo\r\nthree";
    let output = from_stdin(input, &["--lines", "2"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"one\r\ntwo\r\n");

    let output = from_stdin(input, &["--from", "2", "--lines", "2", "-n"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, "2 │ two\n3 │ three\n".as_bytes());

    let output = from_stdin(b"\xff\nvalid\n\xfe", &["--from", "2", "--lines", "1", "-n"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, "2 │ valid\n".as_bytes());

    let output = from_stdin(b"skip\n\xff\n", &["--from", "2", "--lines", "1", "-n"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid utf-8"));

    let max = usize::MAX.to_string();
    let output = from_stdin(input, &["--from", &max, "--lines", &max]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
}

#[test]
fn ranges_apply_to_each_file_and_read_stdin_once() {
    let dir = common::tempdir();
    let first = dir.path().join("first.log");
    let second = dir.path().join("second.log");
    fs::write(&first, b"one\nfirst\r\nextra").expect("prepare or execute range fixture");
    fs::write(&second, b"one\nsecond").expect("prepare or execute range fixture");
    let output = common::rtk_command()
        .arg("read")
        .args([&first, &second])
        .args(["--from", "2", "--lines", "1"])
        .output()
        .expect("prepare or execute range fixture");
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"first\r\nsecond");

    let output = from_stdin(b"one\ntwo\nthree", &["-", "--from", "2", "--lines", "1"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"two\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("stdin specified more than once"));
}

#[test]
fn ranges_reject_ambiguous_or_invalid_arguments() {
    for args in [
        vec!["--from", "2"],
        vec!["--from", "0", "--lines", "1"],
        vec!["--from", "-1", "--lines", "1"],
        vec!["--lines", "-1"],
        vec!["--lines", "1", "--head-lines", "1"],
        vec!["--lines", "1", "--tail-lines", "1"],
        vec!["--lines", "1", "--max-lines", "1"],
        vec!["--lines", "1", "--level", "minimal"],
        vec!["--lines", "1", "--level", "none"],
    ] {
        let output = common::rtk_command()
            .args(["read", "-"])
            .args(&args)
            .output()
            .expect("prepare or execute range fixture");
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {:?}",
            output.stderr
        );
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn invalid_ranges_with_root_options_still_report_usage_errors() {
    for root in ["-v", "--skip-env"] {
        let output = common::rtk_command()
            .args([root, "read", "-", "--from", "0", "--lines", "1"])
            .output()
            .expect("reject invalid range after root options");
        assert_eq!(output.status.code(), Some(2), "{root}: {:?}", output.stderr);
        assert!(String::from_utf8_lossy(&output.stderr).contains("--from"));
    }
}

#[test]
fn zero_count_still_reports_a_missing_file() {
    let dir = common::tempdir();
    let output = common::rtk_command()
        .arg("read")
        .arg(dir.path().join("missing.log"))
        .args(["--lines", "0"])
        .output()
        .expect("prepare or execute range fixture");
    assert_eq!(output.status.code(), Some(1), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
}

#[test]
fn range_stdin_returns_before_the_writer_closes() {
    let mut child = common::rtk_command()
        .args(["read", "-", "--from", "2", "--lines", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start bounded stdin read");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"skip\nselected\n")
        .expect("write lines");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while child.try_wait().expect("check reader").is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().expect("kill stalled reader");
            child.wait().expect("reap stalled reader");
            panic!("range reader waited for EOF");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("collect range");
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"selected\n");
}

#[test]
fn range_errors_do_not_prevent_later_files_from_being_read() {
    let dir = common::tempdir();
    let file = dir.path().join("valid.log");
    fs::write(&file, b"skip\nselected\n").expect("write fixture");
    let output = common::rtk_command()
        .arg("read")
        .args([dir.path().join("missing"), file])
        .args(["--from", "2", "--lines", "1"])
        .output()
        .expect("read files");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"selected\n");
}

#[test]
fn range_accounting_counts_selected_bytes_without_decoding_them() {
    let dir = common::tempdir();
    let file = dir.path().join("binary.log");
    let db = dir.path().join("history.db");
    let mut input = vec![b'x'; 100_000];
    input.extend_from_slice(b"\n\xff\xfe\xff\xfe\xff\xfe\xff\xfe\n");
    fs::write(&file, input).expect("write binary fixture");
    let output = common::rtk_command()
        .env("RTK_DB_PATH", &db)
        .arg("read")
        .arg(file)
        .args(["--from", "2", "--lines", "1"])
        .output()
        .expect("read binary range");
    assert!(output.status.success(), "{:?}", output.stderr);
    let conn = rusqlite::Connection::open(db).expect("open isolated history");
    let counts: (usize, usize, i64) = conn.query_row(
        "SELECT input_tokens, output_tokens, saved_tokens FROM commands WHERE rtk_cmd = 'rtk read --lines'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    ).expect("range accounting row");
    assert_eq!(counts, (3, 3, 0));
}

#[cfg(feature = "engine")]
#[test]
fn ranges_honour_file_policy_even_for_zero_count() {
    let dir = common::tempdir();
    let config = dir.path().join("config");
    fs::create_dir(&config).expect("create isolated config");
    fs::write(config.join("pipeline.toml"), "[pipeline]\nname = 'range-policy'\n[[hook.before_tool.read]]\nname = 'deny-file'\naction = 'deny'\nmessage = 'range denied'\nwhen = { basename = 'denied.log' }\n").expect("write policy");
    // The file need not exist: policy denial must run before File::open, even for count 0.
    for count in ["0", "1"] {
        let output = common::rtk_command()
            .env("RTK_CONFIG_DIR", &config)
            .arg("read")
            .arg(dir.path().join("denied.log"))
            .args(["--lines", count])
            .output()
            .expect("read denied range");
        assert_eq!(output.status.code(), Some(1), "{:?}", output.stderr);
        assert_eq!(output.stdout, b"range denied\n");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("No such file"));
    }
}

#[cfg(feature = "engine")]
#[test]
fn ranges_are_evaluated_as_partial_reads_by_policy() {
    let dir = common::tempdir();
    let config = dir.path().join("config");
    fs::create_dir(&config).expect("create isolated config");
    fs::write(config.join("pipeline.toml"), "[pipeline]\nname = 'range-policy'\n[[hook.before_tool.read]]\nname = 'deny-whole'\naction = 'deny'\nmessage = 'whole read denied'\nwhen = { whole_file = true }\n").expect("write policy");
    let file = dir.path().join("fixture.log");
    fs::write(&file, b"skip\nselected\n").expect("write input");
    let mut command = common::rtk_command();
    command
        .env("RTK_CONFIG_DIR", &config)
        .arg("read")
        .arg(&file);
    let whole = command.output().expect("run whole read");
    assert_eq!(whole.status.code(), Some(1));
    assert_eq!(whole.stdout, b"whole read denied\n");
    let range = command
        .args(["--from", "2", "--lines", "1"])
        .output()
        .expect("run partial read");
    assert!(range.status.success(), "{:?}", range.stderr);
    assert_eq!(range.stdout, b"selected\n");
}
