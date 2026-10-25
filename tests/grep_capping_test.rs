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
