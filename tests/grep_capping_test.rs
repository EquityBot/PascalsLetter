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

/// 400 long lines force the grouped form; every shown line must still hold `wanted`.
fn assert_every_clip_keeps(args: &[&str], line: impl Fn(usize) -> String, wanted: &str) {
    assert_every_clip_keeps_in(&[], args, line, wanted);
}

/// [`assert_every_clip_keeps`] with extra environment, such as a locale.
fn assert_every_clip_keeps_in(
    env: &[(&str, &str)],
    args: &[&str],
    line: impl Fn(usize) -> String,
    wanted: &str,
) {
    // CI runners have grep but not always rg; a case for a missing engine has nothing to test.
    let engine_runs = std::process::Command::new(args[0])
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !engine_runs {
        eprintln!("skipped: {} is not installed", args[0]);
        return;
    }
    let dir = tempfile::tempdir().expect("test setup");
    let body: String = (0..400).map(|i| line(i) + "\n").collect();
    std::fs::write(dir.path().join("long.txt"), body).expect("test setup");

    let out = common::rtk_command()
        .args(args)
        .arg("long.txt")
        .envs(env.iter().copied())
        .current_dir(dir.path())
        .output()
        .expect("rtk");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.starts_with("400 matches in 1 files:"),
        "{stdout:.300}"
    );
    let shown: Vec<&str> = stdout.lines().filter(|l| l.contains("fff")).collect();
    assert!(!shown.is_empty(), "{stdout:.300}");
    for l in shown {
        assert!(l.contains(wanted), "clip lost {wanted:?}: {l}");
    }
}

#[test]
fn a_case_sensitive_clip_skips_a_differently_cased_lookalike() {
    let line = |i| format!("A.B {} a.b {} {i}", "f".repeat(240), "g".repeat(240));
    assert_every_clip_keeps(&["grep", "-F", "a.b"], line, "a.b");
}

#[test]
fn a_bre_literal_caret_keeps_its_match() {
    let line = |i| format!("{} a^b {} {i}", "f".repeat(240), "g".repeat(240));
    assert_every_clip_keeps(&["grep", "a^b"], line, "a^b");
}

#[test]
fn a_pcre_lookbehind_keeps_its_match() {
    let pcre = std::process::Command::new("rg")
        .arg("--pcre2-version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !pcre {
        return;
    }
    let line = |i| format!("{} foobar {} {i}", "f".repeat(240), "g".repeat(240));
    assert_every_clip_keeps(&["rg", "--pcre2", "(?<=foo)bar"], line, "foobar");
    // The pattern's own spelling early in the line is no PCRE match.
    let decoy = |i| {
        format!(
            "(?<=foo)bar {} foobar {} {i}",
            "f".repeat(240),
            "g".repeat(240)
        )
    };
    assert_every_clip_keeps(&["rg", "--pcre2", "(?<=foo)bar"], decoy, "foobar");
}

#[test]
fn a_smart_case_clip_keeps_an_escaped_uppercase_match() {
    // rg reads `\x41` as an uppercase literal, so -S searches case-sensitively for `A`.
    let line = |i| format!("a {} A {} {i}", "f".repeat(240), "g".repeat(240));
    assert_every_clip_keeps(&["rg", "-S", r"\x41"], line, " A ");
}

#[test]
fn a_whole_word_clip_keeps_the_standalone_word() {
    let line = |i| format!("needles {} needle {} {i}", "f".repeat(240), "g".repeat(240));
    assert_every_clip_keeps(&["grep", "-w", "needle"], line, " needle ");
    assert_every_clip_keeps(&["rg", "-w", "needle"], line, " needle ");
}

#[test]
fn an_rg_fixed_string_clip_survives_engine_flags() {
    // --no-pcre2 and --engine pick the regex engine; neither turns -F off.
    let line = |i| format!("aXb {} a.b {} {i}", "f".repeat(240), "g".repeat(240));
    assert_every_clip_keeps(&["rg", "-F", "--no-pcre2", "a.b"], line, "a.b");
    assert_every_clip_keeps(&["rg", "-F", "--engine=default", "a.b"], line, "a.b");
}

#[test]
fn a_gnu_only_bre_escape_keeps_its_match() {
    // GNU grep reads \` as the start of the line; Rust's regex as a literal backtick.
    let line = |i| format!("foo {} `foo {} {i}", "f".repeat(240), "g".repeat(240));
    assert_every_clip_keeps(&["grep", r"\`foo"], line, "foo fff");
}

#[test]
fn a_mid_pattern_bre_caret_keeps_its_match() {
    // GNU reads this `^` as a literal; Rust would anchor `foo` at the line start.
    let line = |i| format!("foo {} ^foo {} {i}", "f".repeat(240), "g".repeat(240));
    assert_every_clip_keeps(&["grep", r"a\?^foo"], line, "^foo");
}

#[test]
fn an_anchor_is_not_matched_after_trimming() {
    // grep sees the leading spaces, so `^foo` is no match; the line matched on `needle`.
    let line = |i| format!("  foo {} needle {} {i}", "f".repeat(240), "g".repeat(240));
    assert_every_clip_keeps(&["grep", "-E", "^foo|needle"], line, " needle ");
    assert_every_clip_keeps(&["rg", "^foo|needle"], line, " needle ");
}

#[test]
fn a_c_locale_space_class_keeps_its_match() {
    // In the C locale grep's `\s` is ASCII; an EM SPACE before `needle` is no match (the
    // leading `x` keeps it from being trimmed off the line).
    let line = |i| {
        format!(
            "x\u{2003}needle {} needle {} {i}",
            "f".repeat(240),
            "g".repeat(240)
        )
    };
    let c = [("LC_ALL", "C")];
    assert_every_clip_keeps_in(&c, &["grep", r"\sneedle"], line, "f needle g");
}

#[test]
fn a_utf8_negated_class_keeps_its_match() {
    // In a UTF-8 locale `é` is alphabetic, so `[^[:alpha:]]needle` matches only `5needle`.
    let line = |i| {
        format!(
            "éneedle {} 5needle {} {i}",
            "f".repeat(240),
            "g".repeat(240)
        )
    };
    let utf8 = [("LC_ALL", "C.UTF-8")];
    assert_every_clip_keeps_in(&utf8, &["grep", "[^[:alpha:]]needle"], line, "5needle");
}

#[test]
fn a_whole_line_match_keeps_the_line() {
    // `foo` alone is no -x match; the line matches only as a whole.
    let long = format!("foo {} {}", "f".repeat(240), "g".repeat(240));
    let line = |_| long.clone();
    for engine in ["grep", "rg"] {
        assert_every_clip_keeps(&[engine, "-Fx", "-e", "foo", "-e", &long], line, &long);
    }
}

#[test]
fn a_very_long_unlocated_line_is_cut_and_says_so() {
    // -w gets no locator; a 20,000-char line kept whole would swamp the agent's cap.
    let dir = tempfile::tempdir().expect("test setup");
    let body: String = (0..400)
        .map(|i| format!("{} needle {i}\n", "f".repeat(20_000)))
        .collect();
    std::fs::write(dir.path().join("long.txt"), body).expect("test setup");

    let out = common::rtk_command()
        .args(["grep", "-w", "needle", "long.txt"])
        .current_dir(dir.path())
        .output()
        .expect("rtk");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.starts_with("400 matches in 1 files:"),
        "{stdout:.300}"
    );
    assert!(stdout.chars().count() <= 30_000, "{} chars", stdout.len());
    let shown: Vec<&str> = stdout.lines().filter(|l| l.contains("fff")).collect();
    assert!(!shown.is_empty(), "{stdout:.300}");
    for l in shown {
        assert!(
            l.ends_with("chars cut; match position unknown]"),
            "{l:.300}"
        );
    }
}

#[test]
fn a_match_longer_than_the_window_keeps_its_end() {
    let line = |i| format!("{} needle {} {i}", "f".repeat(240), "g".repeat(240));
    assert_every_clip_keeps(&["rg", ".*needle"], line, "needle");
    assert_every_clip_keeps(&["grep", ".*needle"], line, "needle");
}
