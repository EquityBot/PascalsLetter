//! When capping saves too little, `rtk grep` must print grep's own output: the grouped form
//! hides matches and clips lines, and the agent pays that back with a re-run.

mod common;

#[test]
fn a_small_overflow_of_the_per_file_cap_prints_every_match_in_full() {
    let dir = tempfile::tempdir().expect("test setup");
    // 30 matches in one file overflow the default per-file cap of 25, but hiding 5 lines and
    // clipping the rest saves far less than a third of the output.
    let lines: Vec<String> = (0..30)
        .map(|i| format!("### heading {i:02} {}", "detail ".repeat(20)))
        .collect();
    std::fs::write(dir.path().join("log.md"), lines.join("\n") + "\n").expect("test setup");

    let out = common::rtk_command()
        .args(["grep", "-n", "^### ", "log.md"])
        .current_dir(dir.path())
        .output()
        .expect("rtk grep");
    let stdout = String::from_utf8_lossy(&out.stdout);

    for (i, line) in lines.iter().enumerate() {
        assert!(
            stdout.contains(&format!("{}:{line}", i + 1)),
            "line {} missing or clipped:\n{stdout}",
            i + 1
        );
    }
    assert!(!stdout.contains("more in"), "capped form chosen:\n{stdout}");
}

#[test]
fn a_large_overflow_is_still_capped() {
    let dir = tempfile::tempdir().expect("test setup");
    let body: String = (0..400)
        .map(|i| format!("match {i} {}\n", "z".repeat(300)))
        .collect();
    std::fs::write(dir.path().join("big.txt"), body).expect("test setup");

    let out = common::rtk_command()
        .args(["grep", "match", "big.txt"])
        .current_dir(dir.path())
        .output()
        .expect("rtk grep");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(stdout.starts_with("400 matches in 1 files:"), "{stdout}");
    assert!(
        stdout.len() < 400 * 300 / 3,
        "not capped: {} bytes",
        stdout.len()
    );
}

#[test]
fn a_fixed_string_clip_keeps_the_literal_not_a_regex_lookalike() {
    let dir = tempfile::tempdir().expect("test setup");
    // `a.b` as a regex matches the `aXb` decoy first; grep -F matched the literal.
    let body: String = (0..400)
        .map(|i| format!("aXb {} a.b {} {i}\n", "f".repeat(200), "g".repeat(200)))
        .collect();
    std::fs::write(dir.path().join("long.txt"), body).expect("test setup");

    let out = common::rtk_command()
        .args(["grep", "-F", "a.b", "long.txt"])
        .current_dir(dir.path())
        .output()
        .expect("rtk grep");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.starts_with("400 matches in 1 files:"),
        "{stdout:.300}"
    );
    let shown: Vec<&str> = stdout.lines().filter(|l| l.contains("fff")).collect();
    assert!(!shown.is_empty(), "{stdout:.300}");
    for line in shown {
        assert!(line.contains("a.b"), "clip lost the match: {line}");
    }
}

#[test]
fn grouped_output_is_kept_when_only_it_fits_the_agent_cap() {
    let dir = tempfile::tempdir().expect("test setup");
    // Capping saves well under a third here, but grep's own output is past the 30000-char
    // cap the agent sees, so printing it whole would cut off its tail.
    for f in 0..8 {
        let body: String = (0..27)
            .map(|i| format!("hit {i:02} {}\n", "w".repeat(124)))
            .collect();
        std::fs::write(dir.path().join(format!("f{f}.txt")), body).expect("test setup");
    }

    let native = common::native_command("grep")
        .args(["-rn", "hit", "."])
        .current_dir(dir.path())
        .output()
        .expect("grep");
    assert!(
        native.stdout.len() > 30_000,
        "fixture too small: {}",
        native.stdout.len()
    );

    let out = common::rtk_command()
        .args(["grep", "-rn", "hit", "."])
        .current_dir(dir.path())
        .output()
        .expect("rtk grep");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.starts_with("216 matches in 8 files:"),
        "{stdout:.300}"
    );
    assert!(
        stdout.chars().count() <= 30_000,
        "{} chars",
        stdout.chars().count()
    );
    assert!(
        (stdout.len() as f64) > native.stdout.len() as f64 * 2.0 / 3.0,
        "fixture saves a third or more, so it does not test the cap: {} vs {}",
        stdout.len(),
        native.stdout.len()
    );
}
