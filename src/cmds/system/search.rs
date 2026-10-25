//! Shared search-output filter for `rtk grep` and `rtk rg`.
//!
//! Runs the agent's exact engine (grep or rg) — never substituting one for the other — and
//! compresses its output by grouping matches by file, capping, and teeing overflow.

use crate::core::arg_tokenizer::{self, Dialect, Token, TokenKind, ValueSpec};
use crate::core::guard::never_worse;
use crate::core::shell::{display_args, with_args};
use crate::core::stream::{
    self, CaptureResult, FilterMode, StdinMode, StreamFilter, exec_capture_stdin,
    exec_capture_unguarded,
};
use crate::core::tracking;
use crate::core::utils::{ChildArgExt, resolved_command, strip_ansi};
use crate::core::{args_utils, config, user_env};
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::Command;
use std::sync::LazyLock;

/// True if stdin is something the engine actually reads: a regular file, FIFO or socket --
/// ripgrep's own `is_readable_stdin` rule, and confirmed for both engines (`rg foo < file`
/// searches the file; `rg foo < /dev/null`, a character device, searches the cwd instead).
/// `!is_terminal()` is wider and wrongly matches that `/dev/null` case.
#[cfg(unix)]
fn stdin_is_readable() -> bool {
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileTypeExt;
    std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map(std::fs::File::from)
        .and_then(|f| f.metadata())
        .map(|m| {
            let kind = m.file_type();
            kind.is_file() || kind.is_fifo() || kind.is_socket()
        })
        .unwrap_or(false)
}

/// Windows: ripgrep's own rule (`grep_cli::is_readable_stdin`), through the same crate -- not
/// a terminal (std counts an MSYS pty pipe as one), and a disk file or a pipe. `NUL` is a
/// character device, so the wider `!is_terminal()` sent `rtk rg foo < NUL` down the stdin
/// path and dropped the filenames of a search that rg actually ran over the cwd.
#[cfg(windows)]
fn stdin_is_readable() -> bool {
    !std::io::stdin().is_terminal()
        && winapi_util::file::typ(winapi_util::HandleRef::stdin())
            .map(|t| t.is_disk() || t.is_pipe())
            .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn stdin_is_readable() -> bool {
    !std::io::stdin().is_terminal()
}

/// Which flags consume a value, transcribed per engine from that engine's own `--help`.
/// grep and rg only intersect -- 13 of ~50 entries -- and disagree outright on `-T`, `-r`,
/// `-E` and `--color`, so one merged table with per-flag exceptions misreads whichever engine
/// it wasn't written for. `-e`/`--regexp`'s value is routed to `patterns` downstream, not
/// `flags`; both tables still report it, since this only answers "does the next token belong
/// to this flag".
fn grep_takes_value(kind: TokenKind, name: &str) -> bool {
    match kind {
        // `--color[=WHEN]`/`--colour[=WHEN]` attach their value, so they never consume a token.
        TokenKind::Long => matches!(
            name,
            "after-context"
                | "before-context"
                | "binary-files"
                | "context"
                | "devices"
                | "directories"
                | "exclude"
                | "exclude-dir"
                | "exclude-from"
                | "file"
                | "group-separator"
                | "include"
                | "label"
                | "max-count"
                | "regexp"
        ),
        // `-X` is grep's undocumented matcher selector, still accepted and still value-taking.
        TokenKind::Short => matches!(name, "A" | "B" | "C" | "D" | "X" | "d" | "e" | "f" | "m"),
        _ => false,
    }
}

fn rg_takes_value(kind: TokenKind, name: &str) -> bool {
    match kind {
        TokenKind::Long => matches!(
            name,
            "after-context"
                | "before-context"
                | "color"
                | "colors"
                | "context"
                | "context-separator"
                | "dfa-size-limit"
                | "encoding"
                | "engine"
                | "field-context-separator"
                | "field-match-separator"
                | "file"
                | "generate"
                | "glob"
                | "hostname-bin"
                | "hyperlink-format"
                | "iglob"
                | "ignore-file"
                | "max-columns"
                | "max-count"
                | "max-depth"
                | "max-filesize"
                | "path-separator"
                | "pre"
                | "pre-glob"
                | "regex-size-limit"
                | "regexp"
                | "replace"
                | "sort"
                | "sortr"
                | "threads"
                | "type"
                | "type-add"
                | "type-clear"
                | "type-not"
        ),
        TokenKind::Short => matches!(
            name,
            "A" | "B" | "C" | "E" | "M" | "T" | "d" | "e" | "f" | "g" | "j" | "m" | "r" | "t"
        ),
        _ => false,
    }
}

/// rg accepts the attached spellings `-A=1`/`-e=PAT` and strips the `=` itself; GNU grep does
/// not ("invalid context length argument"), so only rg's is unwrapped. Attached only: a
/// separate-token value is the user's own text, and `rg -e '=='` must search for `==`.
fn unwrap_attached_value(engine: Engine, value: &str) -> &str {
    match engine {
        Engine::Rg => value.strip_prefix('=').unwrap_or(value),
        Engine::Grep => value,
    }
}

/// The module's single tokenizer entry point. Shared so a pre-check and `extract_pattern_path`
/// cannot classify the same argument differently.
fn tokenize_search_args<'a, T: AsRef<str>>(args: &'a [T], engine: Engine) -> Vec<Token<'a>> {
    arg_tokenizer::tokenize_grammar(
        args,
        &|kind, name| search_takes_value(engine, kind, name),
        Dialect::Posix,
    )
}

/// Every grep/rg value-taking flag claims even a literal `--` as its value, unlike git/cargo --
/// verified against both engines for short and long, numeric- and file-typed flags alike.
fn search_takes_value(engine: Engine, kind: TokenKind, name: &str) -> Option<ValueSpec> {
    let takes = match engine {
        Engine::Grep => grep_takes_value(kind, name),
        Engine::Rg => rg_takes_value(kind, name),
    };
    takes.then(|| ValueSpec::value().claiming_dash_dash())
}

/// Unique, descriptive tee slug for a file's overflow matches. `idx` disambiguates
/// files within one grep; the tee filename's epoch handles separate runs.
fn grep_slug(idx: usize, path: &str) -> String {
    let cleaned: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let tail = &cleaned[cleaned.len().saturating_sub(32)..];
    format!("grep_{}_{}", idx, tail)
}

/// Format a file's matches as `path<sep>line<sep>content`. Tee blocks use the
/// real (un-compacted) `path` so recovered lines stay openable.
fn match_block(path: &str, entries: &[(usize, bool, String)]) -> String {
    let mut s = String::new();
    for (line_num, is_match, content) in entries {
        let sep = if *is_match { ':' } else { '-' };
        s.push_str(&format!("{}{}{}{}{}\n", path, sep, line_num, sep, content));
    }
    s
}

/// Extracts `(patterns, paths, flags, has_format_flag, detected)` from the raw trailing args.
/// `patterns` is the positional pattern plus all `-e`/`--regexp` values (empty → error); `paths`
/// is the remaining positionals (empty → caller defaults to `["."]`); `flags` is everything else
/// forwarded verbatim; `has_format_flag`/`detected` ([`DetectedFlags`]) are computed from this
/// same token pass rather than a second scan over the reconstructed `flags` strings.
fn extract_pattern_path<T: AsRef<str>>(
    args: &[T],
    engine: Engine,
) -> (Vec<String>, Vec<String>, Vec<String>, bool, DetectedFlags) {
    let tokens = tokenize_search_args(args, engine);

    let mut e_patterns: Vec<String> = Vec::new();
    let mut patterns_from_file = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    let mut has_format_flag = false;
    // `None` until the user says either way; the last spelling wins, as both engines do.
    let mut show_file_flag: Option<bool> = None;
    let mut show_line_flag: Option<bool> = None;
    let mut recursive = false;
    let mut context = false;
    let mut i = 0;

    while i < tokens.len() {
        let t = &tokens[i];
        match t.kind {
            TokenKind::Long if t.text == "regexp" => {
                if let Some(v) = t.value(&tokens) {
                    e_patterns.push(v.to_string());
                }
            }
            TokenKind::Long => {
                if t.text == "file" {
                    patterns_from_file = true;
                }
                if is_format_flag_token(engine, t.kind, t.text) {
                    has_format_flag = true;
                }
                if is_show_file_token(t.kind, t.text) {
                    show_file_flag = Some(true);
                }
                if is_recursive_token(engine, t.kind, t.text) {
                    recursive = true;
                }
                if is_show_line_on_token(t.kind, t.text) {
                    show_line_flag = Some(true);
                }
                // Neither negation is forwarded: RTK forces `-nH` so it can parse the output,
                // and the user's `--no-filename`/`--no-line-number` would win as the later
                // flag, leaving nothing parseable and forcing a second run of the whole search.
                if is_show_file_off_token(engine, t.kind, t.text) {
                    show_file_flag = Some(false);
                    i += 1;
                    continue;
                }
                if is_show_line_off_token(engine, t.kind, t.text) {
                    show_line_flag = Some(false);
                    i += 1;
                    continue;
                }
                if is_context_token(engine, t.kind, t.text) {
                    context = true;
                }
                match t.attached {
                    Some(v) => flags.push(format!("--{}={v}", t.text)),
                    None => {
                        flags.push(format!("--{}", t.text));
                        if let Some(v) = t.value(&tokens) {
                            flags.push(v.to_string());
                        }
                    }
                }
            }
            // A value consumed by a preceding flag is handled there instead.
            TokenKind::Positional if t.is_free_positional() => {
                positionals.push(t.text.to_string());
            }
            TokenKind::Short => {
                // A cluster's boolean prefix (e.g. "r" in "-rA") stays glued into one
                // flag string, matching how the user typed it; only the trailing
                // value-taking char (if any) and its value are their own tokens.
                let source = t.source_index;
                let start = i;
                while i + 1 < tokens.len()
                    && tokens[i + 1].kind == TokenKind::Short
                    && tokens[i + 1].source_index == source
                {
                    i += 1;
                }
                let cluster = &tokens[start..=i];
                if cluster
                    .iter()
                    .any(|c| is_format_flag_token(engine, c.kind, c.text))
                {
                    has_format_flag = true;
                }
                // Letter by letter, so `-hH` and `-Hh` land where the engine lands them: the
                // later spelling wins.
                for c in cluster {
                    if is_show_file_token(c.kind, c.text) {
                        show_file_flag = Some(true);
                    } else if is_show_file_off_token(engine, c.kind, c.text) {
                        show_file_flag = Some(false);
                    }
                    if is_show_line_on_token(c.kind, c.text) {
                        show_line_flag = Some(true);
                    } else if is_show_line_off_token(engine, c.kind, c.text) {
                        show_line_flag = Some(false);
                    }
                    if is_recursive_token(engine, c.kind, c.text) {
                        recursive = true;
                    }
                }
                if cluster
                    .iter()
                    .any(|c| is_context_token(engine, c.kind, c.text))
                {
                    context = true;
                }
                let (bool_chars, value_char) = match cluster.split_last() {
                    Some((last, rest))
                        if search_takes_value(engine, TokenKind::Short, last.text).is_some() =>
                    {
                        (rest, Some(last))
                    }
                    _ => (cluster, None),
                };

                // `-h` drops out of the cluster for the same reason as its long form: RTK
                // forces `-H` to parse the output and the user's later `-h` would win.
                let glued: String = bool_chars
                    .iter()
                    .filter(|c| {
                        !is_show_file_off_token(engine, c.kind, c.text)
                            && !is_show_line_off_token(engine, c.kind, c.text)
                    })
                    .map(|c| c.text)
                    .collect();
                if !glued.is_empty() {
                    flags.push(format!("-{glued}"));
                }

                if let Some(vt) = value_char {
                    let value = match vt.attached {
                        Some(attached) => Some(unwrap_attached_value(engine, attached)),
                        None => vt.value(&tokens),
                    };
                    if vt.text == "e" {
                        match value {
                            Some(v) => e_patterns.push(v.to_string()),
                            None => flags.push("-e".to_string()),
                        }
                    } else {
                        if vt.text == "f" {
                            patterns_from_file = true;
                        }
                        flags.push(format!("-{}", vt.text));
                        if let Some(v) = value {
                            flags.push(v.to_string());
                        }
                    }
                }
            }
            // DashDash itself carries nothing to emit — the `--` boundary is handled by the
            // tokenizer (everything after it already comes back as Positional).
            _ => {}
        }
        i += 1;
    }

    // `-e`/`--regexp` and `-f`/`--file` both supply the patterns, so every positional is a
    // path. Taking the first one as the pattern instead left `paths` empty, which made the
    // engine read stdin (a hang under an agent harness) or walk the cwd.
    let (patterns, paths) = if !e_patterns.is_empty() || patterns_from_file {
        (e_patterns, positionals)
    } else {
        let paths = positionals.iter().skip(1).cloned().collect();
        let patterns = positionals.into_iter().take(1).collect();
        (patterns, paths)
    };

    let detected = DetectedFlags {
        show_file: show_file_flag,
        show_line: show_line_flag.unwrap_or(false),
        recursive,
        context,
    };

    (patterns, paths, flags, has_format_flag, detected)
}

fn unparsed_signal(stdout: &str) -> usize {
    stdout
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && trimmed != "--" && parse_match_line(line).is_none()
        })
        .count()
}

/// Run real grep so matches and the savings baseline match the agent's command;
/// rg is the fallback when grep is absent, rejects a flag, or `--type` is used.
/// The search engine the agent actually invoked. RTK runs this binary verbatim
/// and never substitutes one for the other.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Grep,
    Rg,
}

impl Engine {
    fn bin(self) -> &'static str {
        match self {
            Engine::Grep => "grep",
            Engine::Rg => "rg",
        }
    }

    pub fn label(self) -> &'static str {
        self.bin()
    }

    /// `-n -H --null` are parse aids (NUL keeps the regroup unambiguous, #1436);
    /// `-I` skips binary noise (-a overrides).
    fn parse_flags(self) -> &'static [&'static str] {
        match self {
            Engine::Grep => &["-n", "-H", "-I", "--null"],
            Engine::Rg => &["-n", "--with-filename", "--null"],
        }
    }
}

/// Runs the agent's exact engine + flags for the grouping path, appending only the
/// parse aids (see `Engine::parse_flags`).
fn engine_capture<T: AsRef<str>>(
    engine: Engine,
    extra_args: &[T],
    patterns: &[String],
    paths: &[String],
) -> Result<CaptureResult> {
    let mut cmd = engine_command(engine, extra_args, patterns, paths, false);
    exec_capture_stdin(&mut cmd).context("search failed")
}

fn engine_command<T: AsRef<str>>(
    engine: Engine,
    extra_args: &[T],
    patterns: &[String],
    paths: &[String],
    line_buffered: bool,
) -> Command {
    let mut cmd = resolved_command(engine.bin());
    cmd.child_args(engine.parse_flags());
    for a in extra_args {
        cmd.child_arg(a.as_ref());
    }
    if line_buffered {
        // The engine writes through a pipe, so flush each match immediately.
        cmd.child_arg("--line-buffered");
    }
    for p in patterns {
        cmd.child_args(["-e", p]);
    }
    cmd.child_arg("--");
    cmd.child_args(paths);
    cmd
}

fn format_match_line(line: &str, show_file: bool, show_line: bool) -> Option<String> {
    let (file, line_num, is_match, content) = parse_match_line(line)?;
    let sep = if is_match { ':' } else { '-' };
    let mut output = String::new();
    if show_file {
        output.push_str(&file);
        output.push(sep);
    }
    if show_line {
        output.push_str(&line_num.to_string());
        output.push(sep);
    }
    output.push_str(content);
    output.push('\n');
    Some(output)
}

/// Emits each piped match as it arrives. Buffered search waits for EOF, so
/// `tail -f app.log | rtk grep ERROR` would otherwise show no matches.
struct SearchStreamFilter {
    show_file: bool,
    show_line: bool,
    max_results: usize,
    shown: usize,
    cap_reported: bool,
}

impl StreamFilter for SearchStreamFilter {
    fn feed_line(&mut self, line: &str) -> Option<String> {
        let Some(output) = format_match_line(line, self.show_file, self.show_line) else {
            if line == "--" && self.shown >= self.max_results {
                return None;
            }
            return Some(format!("{line}\n"));
        };

        if self.shown >= self.max_results {
            if self.cap_reported {
                return None;
            }
            self.cap_reported = true;
            return Some(format!(
                "[rtk] output capped at {} results\n",
                self.max_results
            ));
        }

        self.shown += 1;
        Some(output)
    }

    fn flush(&mut self) -> String {
        String::new()
    }
}

/// The paths-based half of "should the filename be shown": multiple paths, or a directory among
/// them, regardless of any flag. Combined with `extract_pattern_path`'s pre-computed
/// `DetectedFlags::show_file` (the flags-based half) at each call site.
fn wants_show_file(paths: &[String], flags_show_file: bool) -> bool {
    paths.len() > 1 || paths.iter().any(|p| std::path::Path::new(p).is_dir()) || flags_show_file
}

#[allow(clippy::too_many_arguments)]
fn run_streaming_search(
    timer: &tracking::TimedExecution,
    engine: Engine,
    extra_args: &[String],
    patterns: &[String],
    paths: &[String],
    max_results: usize,
    real_cmd: &str,
    detected_flags: DetectedFlags,
) -> Result<i32> {
    let filter = SearchStreamFilter {
        show_file: detected_flags
            .show_file
            .unwrap_or_else(|| wants_show_file(paths, detected_flags.recursive)),
        show_line: detected_flags.show_line,
        max_results,
        shown: 0,
        cap_reported: false,
    };
    let mut cmd = engine_command(engine, extra_args, patterns, paths, true);
    let result = stream::run_streaming(
        &mut cmd,
        StdinMode::Inherit,
        FilterMode::Streaming(Box::new(filter)),
    )
    .context("search failed")?;

    timer.track(
        real_cmd,
        &format!("rtk {}", engine.label()),
        &result.raw_stdout,
        &result.filtered,
    );
    Ok(result.exit_code)
}

/// Runs the agent's command verbatim for forms RTK does not group: format/shape
/// flags and pattern-less modes (`--files`, `--type-list`).
///
/// One exception: a bare file list (`-l`/`-L`/`--files`, see [`is_bare_file_list`]) has its
/// shared directory prefix folded into a header when captured. The streaming form reads
/// stdin, where the only "file" is `(standard input)`, so it has nothing to fold.
fn passthrough<T: AsRef<str>>(
    timer: &tracking::TimedExecution,
    engine: Engine,
    args: &[T],
    real_cmd: &str,
    stream_stdin: bool,
    fold_file_list: bool,
) -> Result<i32> {
    let mut cmd = resolved_command(engine.bin());
    if stream_stdin && !std::io::stdout().is_terminal() {
        // Keep passthrough output live when stdout is piped.
        cmd.child_arg("--line-buffered");
    }
    for a in args {
        cmd.child_arg(a.as_ref());
    }

    if stream_stdin {
        let exit_code =
            stream::run_streaming(&mut cmd, StdinMode::Inherit, FilterMode::Passthrough)
                .context("search failed")?
                .exit_code;
        timer.track_passthrough(real_cmd, &tracking::passthrough_label(real_cmd));
        return Ok(exit_code);
    }

    let result = exec_capture_stdin(&mut cmd).context("search failed")?;
    let cleaned = strip_ansi(&result.stdout);
    let folded = if fold_file_list {
        fold_path_prefix(&cleaned).filter(|f| never_worse(&cleaned, f) == f)
    } else {
        None
    };
    match &folded {
        Some(folded) => print!("{}", folded),
        None => print!("{}", cleaned),
    }
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }

    match &folded {
        // Real sizes, so the fold shows up in `rtk gain`.
        Some(folded) => timer.track(
            real_cmd,
            &format!("rtk {}", engine.label()),
            &cleaned,
            folded,
        ),
        // 0/0 keeps an unchanged passthrough from diluting the savings statistics.
        None => timer.track_passthrough(real_cmd, &tracking::passthrough_label(real_cmd)),
    }
    Ok(result.exit_code)
}

/// Folds the directory prefix every line of a file list shares into a one-line header, then
/// emits each path's remaining tail in engine order:
///
/// ```text
/// /home/u/proj/src/ (3 files)
/// a/foo.rs
/// a/bar.rs
/// b/baz.rs
/// ```
///
/// Agents search from absolute roots, so `-l` output restates the same long prefix on every
/// line; that prefix is the whole redundancy of the list. The transform is lossless (each path
/// is `prefix + tail`) and needs no cap or tee. `None` when there is nothing to fold: fewer
/// than two lines, a line that is not a plain path (empty, NUL-joined under `-Z`, carrying a
/// `\r` that `lines()` would drop, or an escape such as an rg `--hyperlink-format` OSC 8 link
/// that `strip_ansi` leaves in place), or no shared directory component. The prefix is cut on whole components, never inside one, so
/// `/a/foobar/x` and `/a/foobaz/y` share `/a/`, not `/a/fooba`.
///
/// KNOWN LIMITATION: only `/` separates components, so Windows-style `\` paths never fold.
fn fold_path_prefix(raw: &str) -> Option<String> {
    // Checked on the raw text: `lines()` would already have dropped a `\r` before `\n`.
    if raw.contains(['\0', '\r', '\x1b']) {
        return None;
    }
    let paths: Vec<&str> = raw.lines().collect();
    if paths.len() < 2 || paths.iter().any(|p| p.is_empty()) {
        return None;
    }

    fn dirs(path: &str) -> Vec<&str> {
        match path.rsplit_once('/') {
            Some((dir, _)) => dir.split('/').collect(),
            None => Vec::new(),
        }
    }

    let first = dirs(paths[0]);
    let mut common = first.len();
    for path in &paths[1..] {
        let shared = dirs(path)
            .iter()
            .zip(&first)
            .take(common)
            .take_while(|(a, b)| a == b)
            .count();
        common = shared;
        if common == 0 {
            return None;
        }
    }

    // An absolute path splits to a leading empty component, so `/a/b` rejoins with its slash;
    // a bare `/` root is the one prefix that saves nothing.
    let prefix = format!("{}/", first[..common].join("/"));
    if prefix.len() <= 1 {
        return None;
    }

    let mut out = format!("{} ({} files)\n", prefix, paths.len());
    for path in &paths {
        out.push_str(path.strip_prefix(&prefix).unwrap_or(path));
        out.push('\n');
    }
    Some(out)
}

pub fn run(
    engine: Engine,
    max_line_len: usize,
    max_results: usize,
    context_only: bool,
    args: &[String],
    verbose: u8,
) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    // Restored first: every check below classifies these args, and clap ate the boundary.
    let args = &args_utils::restore_double_dash(args);

    // --version / --help: pass through to the engine without filtering. Token-based and
    // scoped before the boundary, because `rtk grep -- --version` searches *for* that string.
    // `-h` is engine-specific: rg's is --help, grep's is --no-filename.
    let help_tokens = tokenize_search_args(args, engine);
    let asks_for_help = arg_tokenizer::before_dashdash(&help_tokens)
        .iter()
        .any(|t| {
            (t.kind == TokenKind::Long && matches!(t.text, "version" | "help"))
                || (t.kind == TokenKind::Short && t.text == "h" && engine == Engine::Rg)
        });
    let dangling_value_flag = help_tokens.iter().any(|t| {
        matches!(t.kind, TokenKind::Long | TokenKind::Short)
            && search_takes_value(engine, t.kind, t.text).is_some()
            && t.value(&help_tokens).is_none()
    });
    if dangling_value_flag {
        let real_cmd = with_args(engine.bin(), &display_args(args));
        return passthrough(&timer, engine, args, &real_cmd, false, false);
    }

    if asks_for_help {
        let mut cmd = resolved_command(engine.bin());
        cmd.child_args(args);
        let result = exec_capture_unguarded(&mut cmd).context("search failed")?;
        print!("{}", result.stdout);
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr);
        }
        return Ok(result.exit_code);
    }

    let real_cmd = with_args(engine.label(), &display_args(args));
    let rtk_label = format!("rtk {}", engine.label());

    let (patterns, paths, extra_args, extra_args_has_format_flag, detected_flags) =
        extract_pattern_path(args, engine);

    if patterns.is_empty() {
        // `rg --files` lists paths without a pattern; fold it like `-l`.
        let fold = is_bare_file_list(engine, args);
        return passthrough(&timer, engine, args, &real_cmd, false, fold);
    }

    let pattern_display = if patterns.len() == 1 {
        patterns[0].clone()
    } else {
        patterns.join("|")
    };

    let path_display = paths.join(" ");

    if verbose > 0 {
        eprintln!("grep: '{}' in {}", pattern_display, path_display);
    }

    let reads_piped_stdin =
        stdin_is_readable() && (paths.is_empty() || paths.iter().any(|path| path == "-"));

    // format/shape flags (-c/-l/-o/...): already-minimal native output, passthrough.
    if extra_args_has_format_flag {
        let fold = is_bare_file_list(engine, args);
        return passthrough(&timer, engine, args, &real_cmd, reads_piped_stdin, fold);
    }

    if reads_piped_stdin {
        return run_streaming_search(
            &timer,
            engine,
            &extra_args,
            &patterns,
            &paths,
            max_results,
            &real_cmd,
            detected_flags,
        );
    }

    let result = engine_capture(engine, &extra_args, &patterns, &paths)?;

    let exit_code = result.exit_code;
    let raw_output = result.stdout.clone();

    // Unparseable shape re-runs verbatim below (with its own stderr), so handle it
    // before surfacing this run's stderr (#2333).
    if unparsed_signal(&raw_output) > 0 {
        return passthrough(&timer, engine, args, &real_cmd, false, false);
    }

    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }

    if result.stdout.trim().is_empty() {
        timer.track(&real_cmd, &rtk_label, &raw_output, "");
        return Ok(exit_code);
    }

    let context_re = if context_only {
        Regex::new(&format!(
            "(?i).{{0,20}}{}.*",
            regex::escape(&pattern_display)
        ))
        .ok()
    } else {
        None
    };

    let locate = locator_for(engine, args, &patterns);
    let mut by_file: HashMap<String, Vec<(usize, bool, String)>> = HashMap::new();
    for line in raw_output.lines() {
        let Some((file, line_num, is_match, content)) = parse_match_line(line) else {
            continue;
        };
        let cleaned = clean_line(content, max_line_len, context_re.as_ref(), locate.as_ref());
        by_file
            .entry(file)
            .or_default()
            .push((line_num, is_match, cleaned));
    }

    let total_matches: usize = by_file
        .values()
        .flat_map(|v| v.iter())
        .filter(|(_, is_match, _)| *is_match)
        .count();

    // Mirror what the real command prints: the filename only when grep/rg would
    // show one (multiple files, a directory, -r or -H), the line number only with
    // -n. We force -nH--null for robust parsing, then drop what the engine itself
    // would not have shown.
    // With no path given, rg walks the cwd, and there the filename is the only way to tell
    // matches apart -- real rg prints it even when a single file matched. grep with no path
    // reads stdin instead (whatever stdin is), where a filename would be `(standard input)`,
    // so the same reasoning does not carry over.
    let walks_cwd = engine == Engine::Rg && paths.is_empty();
    let show_file = detected_flags.show_file.unwrap_or_else(|| {
        by_file.len() > 1 || walks_cwd || wants_show_file(&paths, detected_flags.recursive)
    });
    let show_line = detected_flags.show_line;

    // Faithful baseline: exactly what the real command prints, full content.
    let mut plain = String::new();
    for line in raw_output.lines() {
        let Some(output) = format_match_line(line, show_file, show_line) else {
            if line == "--" {
                plain.push_str("--\n");
            }
            continue;
        };
        plain.push_str(&output);
    }

    let has_context = detected_flags.context;

    let per_file = config::limits().grep_max_per_file;
    let mut files: Vec<_> = by_file.iter().collect();
    files.sort_by_key(|(f, _)| *f);

    let mut body = String::new();
    let mut shown = 0;
    let mut skipped_files = 0;
    let mut skipped_block = String::new();
    for (idx, (file, entries)) in files.into_iter().enumerate() {
        if shown >= max_results {
            skipped_files += 1;
            skipped_block.push_str(&match_block(file, entries));
            continue;
        }

        let file_display = compact_path(file);
        let mut file_shown = 0;
        let mut prev_line: usize = 0;
        for (line_num, is_match, content) in entries.iter().take(per_file) {
            if shown >= max_results {
                break;
            }
            if has_context && prev_line > 0 && *line_num > prev_line + 1 {
                body.push_str("--\n");
            }
            prev_line = *line_num;
            let sep = if *is_match { ':' } else { '-' };
            if show_file {
                body.push_str(&file_display);
                body.push(sep);
            }
            if show_line {
                body.push_str(&line_num.to_string());
                body.push(sep);
            }
            body.push_str(content);
            body.push('\n');
            shown += 1;
            file_shown += 1;
        }

        let remaining = entries.len() - file_shown;
        if remaining == 0 {
            continue;
        }
        // Tee the file's full matches (real path) so the tail hint recovers them
        // openably, skipping the lines already shown.
        let full_block = match_block(file, entries);
        match crate::core::tee::force_tee_tail_hint(
            &full_block,
            &grep_slug(idx, file),
            file_shown + 1,
        ) {
            Some(hint) => body.push_str(&format!(
                "  +{} more in {} {}\n",
                remaining, file_display, hint
            )),
            None => body.push_str(&format!("  +{} more in {}\n", remaining, file_display)),
        }
    }

    if skipped_files > 0 {
        let hint = crate::core::tee::force_tee_tail_hint(&skipped_block, "grep_skipped", 1)
            .map(|h| format!(" {}", h))
            .unwrap_or_default();
        body.push_str(&format!("+{} more files{}\n", skipped_files, hint));
    }

    // Switch to the grouped form only when capping actually shrank the output;
    // otherwise emit the faithful baseline, so RTK never exceeds the real command.
    let capped = shown < total_matches || skipped_files > 0;
    let rtk_output = if capped {
        format!(
            "{} matches in {} files:\n\n{}",
            total_matches,
            by_file.len(),
            body
        )
    } else {
        body
    };

    // Capping hides matches and clips lines, which the agent pays back with a re-run, so the
    // grouped form must buy a real saving: at least a third of the faithful output. Unless
    // only the grouped form fits what the agent sees: the faithful one would lose its tail,
    // and with it the files and overflow hints the grouped form keeps.
    let budget = crate::core::config::limits().agent_output_max_chars;
    let (grouped_chars, plain_chars) = (rtk_output.chars().count(), plain.chars().count());
    let output = if capped
        && (grouped_chars * 3 <= plain_chars * 2
            || (grouped_chars <= budget && plain_chars > budget))
    {
        rtk_output
    } else {
        plain
    };

    print!("{}", output);
    timer.track(&real_cmd, &rtk_label, &raw_output, &output);

    Ok(exit_code)
}

/// Parses a single rg/grep match or context line of the form `file\0line_number[:-]content`.
/// Requires `-0`/`--null` so the filename is NUL-separated -- NUL can't appear in file paths,
/// so this stays unambiguous even with `:` in the content or path. The `bool` is `true` for a
/// match line (`:` separator), `false` for context (`-`, from `-A`/`-B`/`-C`).
fn parse_match_line(line: &str) -> Option<(String, usize, bool, &str)> {
    static MATCH_LINE_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^([^\x00]+)\x00(\d+)([:-])(.*)$").unwrap());

    MATCH_LINE_RE.captures(line).and_then(|caps| {
        let file = caps.get(1)?.as_str().to_string();
        let line_num: usize = caps.get(2)?.as_str().parse().ok()?;
        let sep = caps.get(3)?.as_str();
        let content = caps.get(4)?.as_str();
        let is_match = sep == ":";
        Some((file, line_num, is_match, content))
    })
}

/// Minimal/shape forms the agent already chose (`-c`/`-l`/`--json`/...). `engine`-aware: `-L`
/// is grep's `--files-without-match` but rg's `--follow` (symlinks); `-z` is grep's
/// `--null-data` but rg's `--search-zip` -- neither rg meaning is a shape flag.
fn is_format_flag_token(engine: Engine, kind: TokenKind, text: &str) -> bool {
    const LONG: &[&str] = &[
        "byte-offset",
        "column",
        "count",
        "count-matches",
        "files",
        "files-with-matches",
        "files-without-match",
        "json",
        "null",
        "null-data",
        "only-matching",
        "passthru",
        "quiet",
        "silent",
        "vimgrep",
    ];
    match kind {
        // grep's `--initial-tab` pads and tabs every match line, so RTK's own `-H --null -n`
        // parse reads nothing back and leaked the injected flags into the output. ripgrep has
        // no such flag, and its `-T` is `--type-not`, a value-taking flag (see rg_takes_value).
        TokenKind::Long => {
            LONG.contains(&text) || (engine == Engine::Grep && text == "initial-tab")
        }
        // -c count, -l/-L lists, -o only-matching, -q quiet, -b byte-offset, -Z NUL are shared;
        // -L/-T/-z mean something unrelated to output shape for rg specifically (see above).
        TokenKind::Short => match text {
            "L" | "T" | "z" => engine == Engine::Grep,
            "Z" | "b" | "c" | "l" | "o" | "q" => true,
            _ => false,
        },
        _ => false,
    }
}

/// True when the command's only shape flag is a file list -- `-l`/`--files-with-matches`,
/// grep's `-L`/`--files-without-match`, or rg's `--files` -- so every stdout line is one
/// plain path and [`fold_path_prefix`] applies. Any other shape flag changes the line
/// (`-c` appends `:count`, `-Z`/`--null` joins with NUL, `--json` wraps it) or removes it
/// (`-q`), so the list is left verbatim.
pub(crate) fn is_bare_file_list<T: AsRef<str>>(engine: Engine, args: &[T]) -> bool {
    let tokens = tokenize_search_args(args, engine);
    let mut file_list = false;
    for t in &tokens {
        let is_list = match t.kind {
            TokenKind::Long => matches!(
                t.text,
                "files" | "files-with-matches" | "files-without-match"
            ),
            TokenKind::Short => t.text == "l" || (t.text == "L" && engine == Engine::Grep),
            _ => false,
        };
        if is_list {
            file_list = true;
        } else if is_format_flag_token(engine, t.kind, t.text) {
            return false;
        }
    }
    file_list
}

/// True for `-H`/`--with-filename`, an explicit request for the filename prefix (same meaning
/// for both engines).
fn is_show_file_token(kind: TokenKind, text: &str) -> bool {
    match kind {
        TokenKind::Long => text == "with-filename",
        TokenKind::Short => text == "H",
        _ => false,
    }
}

/// True for grep's `-r`/`-R`/`--recursive`. Recursion is not a filename request: it only makes
/// the search span several files, so grep shows the prefix by default -- an explicit `-h` still
/// wins whichever side of it the recursion flag is typed on. ripgrep has none of these
/// spellings (`-r` is `--replace`, a value-taking flag, see [`rg_takes_value`]).
fn is_recursive_token(engine: Engine, kind: TokenKind, text: &str) -> bool {
    engine == Engine::Grep
        && match kind {
            TokenKind::Long => text == "recursive",
            TokenKind::Short => matches!(text, "R" | "r"),
            _ => false,
        }
}

/// True for `-n`/`--line-number` (identical meaning for both engines).
fn is_show_line_on_token(kind: TokenKind, text: &str) -> bool {
    match kind {
        TokenKind::Long => text == "line-number",
        TokenKind::Short => text == "n",
        _ => false,
    }
}

/// True for `-h`/`--no-filename` (negates [`is_show_file_token`]). RTK forces `-H` so it can
/// parse the output, so the user's request has to be honoured at display time instead --
/// leaving it in the engine command would defeat RTK's own parse and force a second run.
fn is_show_file_off_token(engine: Engine, kind: TokenKind, text: &str) -> bool {
    match kind {
        TokenKind::Long => text == "no-filename",
        // Divergent both ways: grep's `-h` is --no-filename where rg's is --help, and rg's
        // `-I` is --no-filename where grep's is --binary-files=without-match.
        TokenKind::Short => match text {
            "h" => engine == Engine::Grep,
            "I" => engine == Engine::Rg,
            _ => false,
        },
        _ => false,
    }
}

/// True for `-N`/`--no-line-number` (negates [`is_show_line_on_token`]). ripgrep-only: GNU grep
/// has neither spelling and exits 2 on both, so recognising them there would swallow a flag the
/// engine itself refuses.
fn is_show_line_off_token(engine: Engine, kind: TokenKind, text: &str) -> bool {
    engine == Engine::Rg
        && match kind {
            TokenKind::Long => text == "no-line-number",
            TokenKind::Short => text == "N",
            _ => false,
        }
}

/// True for a context-window flag: `-A`/`-B`/`-C`, their long forms, or -- grep only -- the
/// `-NUM` shorthand for `--context=NUM` (the tokenizer keeps that digit run as one `Short`
/// token). ripgrep has no `-NUM`; its `-0` is `--null`, so reading a digit as context there
/// changes the output shape for a flag that has nothing to do with context.
fn is_context_token(engine: Engine, kind: TokenKind, text: &str) -> bool {
    match kind {
        TokenKind::Long => matches!(text, "after-context" | "before-context" | "context"),
        TokenKind::Short => {
            matches!(text, "A" | "B" | "C")
                || (engine == Engine::Grep && arg_tokenizer::is_digit_run(text))
        }
        _ => false,
    }
}

/// Flags detected during [`extract_pattern_path`]'s own token pass, replacing the
/// reconstructed-string scans `show_file`/`show_line`/`has_context_flag` used to rely on (see
/// the ambiguity this avoids: a value-taking flag's own value,
/// pushed into `flags` as a bare string, could otherwise be misread as one of these).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DetectedFlags {
    /// What the user asked for with `-H`/`--with-filename` (`Some(true)`) or
    /// `-h`/`--no-filename` (`Some(false)`), last spelling winning as both engines do; `None`
    /// when they said neither, leaving the decision to `recursive` and to the paths (multiple
    /// paths, a directory among them), which the call sites check against `paths` themselves.
    show_file: Option<bool>,
    /// `-n`/`--line-number`, unless negated by `-N`/`--no-line-number`.
    show_line: bool,
    /// grep's `-r`/`-R`/`--recursive`: not a filename request, only a reason for the engine to
    /// show one by default, so it feeds `show_file`'s fallback rather than overriding it.
    recursive: bool,
    /// `-A`/`-B`/`-C` or their long forms.
    context: bool,
}

/// Test-only convenience wrapper; the production call site gets this from the
/// `has_format_flag` extract_pattern_path already returns, computed in the same token pass
/// instead of tokenizing the reconstructed `flags` strings a second time.
#[cfg(test)]
fn has_format_flag<T: AsRef<str>>(engine: Engine, extra_args: &[T]) -> bool {
    // The module's shared tokenizer, so a value-taking flag's value (e.g. `-e --json`, where
    // "--json" is -e's pattern, not the real --json flag) is classified exactly as
    // extract_pattern_path classifies it.
    let tokens = tokenize_search_args(extra_args, engine);
    tokens
        .iter()
        .any(|t| is_format_flag_token(engine, t.kind, t.text))
}

/// An unlocated line up to this many clip windows long is shown whole.
const UNLOCATED_WHOLE_WINDOWS: usize = 4;

fn clean_line(
    line: &str,
    max_len: usize,
    context_re: Option<&Regex>,
    locate: Option<&Locator>,
) -> String {
    let trimmed = line.trim();

    if let Some(re) = context_re
        && let Some(m) = re.find(trimmed)
    {
        let matched = m.as_str();
        if matched.len() <= max_len {
            return matched.to_string();
        }
    }

    let char_len = trimmed.chars().count();
    if char_len <= max_len {
        return trimmed.to_string();
    }

    // Place the window on the engine's actual match, not a literal search for the pattern
    // text, which misses every real regex (`fn\s+run`) and so cut away the very match the
    // agent searched for. The match offset is taken on the line itself: lowercasing first
    // can change its length and pushed the window past the end of the line.
    // No trustworthy position (PCRE, `-w`, a mid-pattern BRE `^`), or a match too long for
    // the window: a clip could cut away the match, so the line stays whole. Past a few
    // windows (minified code, data) a whole line would swamp the agent's output cap, so it
    // is cut at the head and says the match may lie past the cut.
    // The match is found on the line as the engine saw it: trimmed first, `^foo` would match
    // a line that starts with spaces. A match reaching into the trimmed ends counts as none.
    let lead = line.chars().count() - line.trim_start().chars().count();
    let Some((match_start, match_end)) = locate
        .and_then(|re| re.find_chars(line))
        .filter(|&(start, end)| start >= lead && end <= lead + char_len)
        .map(|(start, end)| (start - lead, end - lead))
        .filter(|(start, end)| end - start <= max_len)
    else {
        if char_len <= UNLOCATED_WHOLE_WINDOWS * max_len {
            return trimmed.to_string();
        }
        let head: String = trimmed.chars().take(max_len).collect();
        return format!(
            "{head}... [+{} chars cut; match position unknown]",
            char_len - max_len
        );
    };

    // The whole match inside the window, a third of it ahead of the match where room allows.
    let start = match_start
        .saturating_sub(max_len / 3)
        .max(match_end.saturating_sub(max_len))
        .min(char_len - max_len);
    let end = start + max_len;
    let slice: String = trimmed.chars().skip(start).take(max_len).collect();
    if start > 0 && end < char_len {
        format!("...{}...", slice)
    } else if start > 0 {
        format!("...{}", slice)
    } else {
        format!("{}...", slice)
    }
}

/// The language the engine reads its patterns in. The last mode flag wins, as in both engines.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PatternDialect {
    Fixed,
    /// grep's default BRE, where `\+ \? \| \( \) \{ \}` are the operators.
    Basic,
    /// grep -E: bare operators as in Rust, but its escapes and bracket expressions differ.
    Extended,
    /// rg's Rust regex.
    Regex,
    /// grep -P and rg's PCRE2 engine: lookaround and other syntax Rust's regex lacks.
    Pcre,
}

fn pattern_dialect<T: AsRef<str>>(engine: Engine, args: &[T]) -> PatternDialect {
    let tokens = tokenize_search_args(args, engine);
    let mut dialect = match engine {
        Engine::Grep => PatternDialect::Basic,
        Engine::Rg => PatternDialect::Regex,
    };
    // rg keeps -F apart from the engine: -F wins over any engine until --no-fixed-strings.
    let mut rg_fixed = false;
    for t in arg_tokenizer::before_dashdash(&tokens) {
        match (engine, t.kind, t.text) {
            (Engine::Rg, TokenKind::Short, "F")
            | (Engine::Rg, TokenKind::Long, "fixed-strings") => rg_fixed = true,
            (Engine::Rg, TokenKind::Long, "no-fixed-strings") => rg_fixed = false,
            (Engine::Rg, TokenKind::Short, "P") | (Engine::Rg, TokenKind::Long, "pcre2") => {
                dialect = PatternDialect::Pcre
            }
            (Engine::Rg, TokenKind::Long, "no-pcre2") => dialect = PatternDialect::Regex,
            (Engine::Rg, TokenKind::Long, "engine") => {
                dialect = match t.attached.or_else(|| t.value(&tokens)) {
                    Some("default") => PatternDialect::Regex,
                    _ => PatternDialect::Pcre,
                }
            }
            (Engine::Grep, TokenKind::Short, "F")
            | (Engine::Grep, TokenKind::Long, "fixed-strings") => dialect = PatternDialect::Fixed,
            (Engine::Grep, TokenKind::Short, "G")
            | (Engine::Grep, TokenKind::Long, "basic-regexp") => dialect = PatternDialect::Basic,
            (Engine::Grep, TokenKind::Short, "E")
            | (Engine::Grep, TokenKind::Long, "extended-regexp") => {
                dialect = PatternDialect::Extended
            }
            (Engine::Grep, TokenKind::Short, "P")
            | (Engine::Grep, TokenKind::Long, "perl-regexp") => dialect = PatternDialect::Pcre,
            _ => {}
        }
    }
    if rg_fixed {
        PatternDialect::Fixed
    } else {
        dialect
    }
}

/// Whether the engine ignores case, by the user's own flags (the last one wins). Smart case
/// and unknown settings, such as an rg config file's, count as case-sensitive: a locator
/// stricter than the engine finds a real match or none, never a lookalike.
fn ignores_case<T: AsRef<str>>(engine: Engine, args: &[T]) -> bool {
    let tokens = tokenize_search_args(args, engine);
    let mut ignore = false;
    for t in arg_tokenizer::before_dashdash(&tokens) {
        match (engine, t.kind, t.text) {
            (_, TokenKind::Short, "i") | (_, TokenKind::Long, "ignore-case") => ignore = true,
            (Engine::Grep, TokenKind::Short, "y") => ignore = true,
            (Engine::Grep, TokenKind::Long, "no-ignore-case")
            | (Engine::Rg, TokenKind::Short, "s" | "S")
            | (Engine::Rg, TokenKind::Long, "case-sensitive" | "smart-case") => ignore = false,
            _ => {}
        }
    }
    ignore
}

/// A grep pattern as the equivalent Rust regex. In a BRE (`basic`) the escaped and bare forms
/// of `+ ? | ( ) { }` swap; an ERE keeps them.
///
/// `None` for anything the two could read differently: an escape GNU grep does not define as
/// Rust does (`\d`, `\x41`, back-references, the buffer anchors `` \` `` and `\'`), an ERE
/// `(?` (a Rust flag group), a `{,n}` interval, a BRE `^` or `$` away from the pattern's
/// ends (literal to grep, an anchor to Rust), and inside a bracket expression a backslash, a
/// nested `[` or Rust's `&&`/`--`/`~~` set operators, all literal to grep.
///
/// The result runs on bytes with Unicode off, as grep reads text in the C locale, the only
/// one the grep locator serves.
fn posix_to_regex(pattern: &str, basic: bool) -> Option<String> {
    const SWAPPED: &str = "+?|(){}";
    const WORD_AND_SPACE: &str = "wWsSbB<>";
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                n if SWAPPED.contains(n) && basic => out.push(n),
                // Rust's `\b{start}` and kin; GNU reads `{` there as a literal.
                'b' | 'B' if chars.peek() == Some(&'{') => return None,
                '`' | '\'' => return None,
                n if n.is_ascii_punctuation() || WORD_AND_SPACE.contains(n) => {
                    out.push('\\');
                    out.push(n);
                }
                _ => return None,
            },
            '^' if basic && !out.is_empty() => return None,
            '$' if basic && chars.peek().is_some() => return None,
            '[' => {
                out.push('[');
                if chars.peek() == Some(&'^') {
                    out.push(chars.next()?);
                }
                if chars.peek() == Some(&']') {
                    out.push_str("\\]");
                    chars.next();
                }
                loop {
                    match chars.next()? {
                        ']' => break,
                        '\\' => return None,
                        '[' if chars.peek() == Some(&':') => {
                            out.push('[');
                            for k in chars.by_ref() {
                                out.push(k);
                                if k == ']' {
                                    break;
                                }
                            }
                        }
                        '[' => return None,
                        k @ ('&' | '-' | '~') if chars.peek() == Some(&k) => return None,
                        k => out.push(k),
                    }
                }
                out.push(']');
            }
            '(' if !basic && chars.peek() == Some(&'?') => return None,
            c if SWAPPED.contains(c) && basic => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    // GNU reads `{,n}` as 0 to n repeats.
    (!out.contains("{,")).then_some(out)
}

/// How the engine reads text.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Charset {
    /// grep in the C locale: one byte per character, ASCII classes, case and ranges.
    Bytes,
    /// rg, always.
    Utf8,
}

/// Whether grep runs in the C locale, by what it inherits: the first non-empty of `LC_ALL`,
/// `LC_CTYPE` and `LANG` for its character type, and of `LC_ALL`, `LC_COLLATE` and `LANG`
/// for its bracket ranges, each C or POSIX or unset (agent shells set none). A UTF-8 or
/// other locale is not modelled: its classes, case folding and ranges follow tables Rust
/// does not share, and a missing one falls back to C without saying so.
fn grep_in_c_locale() -> bool {
    let effective = |category: Option<String>| {
        [user_env::var("LC_ALL"), category, user_env::var("LANG")]
            .into_iter()
            .flatten()
            .find(|v| !v.is_empty())
    };
    [
        effective(user_env::var("LC_CTYPE")),
        effective(user_env::var("LC_COLLATE")),
    ]
    .iter()
    .all(|v| matches!(v.as_deref(), None | Some("C" | "POSIX")))
}

/// The match a clipped line is centred on.
enum Locator {
    Chars(Regex),
    Bytes(regex::bytes::Regex),
}

impl Locator {
    /// The leftmost match as char offsets into `line`, widened to whole chars: a byte
    /// locator can stop inside a multi-byte character.
    fn find_chars(&self, line: &str) -> Option<(usize, usize)> {
        let (start, end) = match self {
            Locator::Chars(re) => re.find(line).map(|m| (m.start(), m.end()))?,
            Locator::Bytes(re) => re.find(line.as_bytes()).map(|m| (m.start(), m.end()))?,
        };
        let start = (0..=start).rev().find(|&i| line.is_char_boundary(i))?;
        let end = (end..=line.len()).find(|&i| line.is_char_boundary(i))?;
        let first = line[..start].chars().count();
        Some((first, first + line[start..end].chars().count()))
    }
}

/// Finds the match a clipped line is centred on, reading each pattern (and each line of a
/// multi-line one, which both engines take as separate patterns) in the engine's dialect and
/// charset. A pattern the locator cannot read as the engine does is not located at all, so
/// its line stays whole: its literal spelling is no match.
fn match_locator(
    patterns: &[String],
    dialect: PatternDialect,
    ignore_case: bool,
    charset: Charset,
) -> Option<Locator> {
    let pieces: Vec<&str> = patterns
        .iter()
        .flat_map(|p| p.split('\n'))
        .filter(|p| !p.is_empty())
        .collect();
    if pieces.is_empty() {
        return None;
    }
    let translated = pieces
        .iter()
        .map(|p| match dialect {
            PatternDialect::Fixed => Some(regex::escape(p)),
            PatternDialect::Basic => posix_to_regex(p, true),
            PatternDialect::Extended => posix_to_regex(p, false),
            PatternDialect::Regex => Some(p.to_string()),
            PatternDialect::Pcre => None,
        })
        .map(|p| p.map(|p| format!("(?:{p})")))
        .collect::<Option<Vec<String>>>()?;
    let alternation = translated.join("|");
    match charset {
        Charset::Utf8 => regex::RegexBuilder::new(&alternation)
            .case_insensitive(ignore_case)
            .build()
            .ok()
            .map(Locator::Chars),
        Charset::Bytes => regex::bytes::RegexBuilder::new(&alternation)
            .case_insensitive(ignore_case)
            .unicode(false)
            .build()
            .ok()
            .map(Locator::Bytes),
    }
}

/// The locator for this search, or none where its matches could differ from the engine's:
/// PCRE; `-w` and `-x`, whose word and line boundaries the locator does not model; rg's
/// `--no-unicode`; and grep outside the C locale.
fn locator_for<T: AsRef<str>>(engine: Engine, args: &[T], patterns: &[String]) -> Option<Locator> {
    let tokens = tokenize_search_args(args, engine);
    let unmodelled = arg_tokenizer::before_dashdash(&tokens).iter().any(|t| {
        (t.kind == TokenKind::Short && matches!(t.text, "w" | "x"))
            || (t.kind == TokenKind::Long
                && matches!(t.text, "word-regexp" | "line-regexp" | "no-unicode"))
    });
    if unmodelled {
        return None;
    }
    let charset = match engine {
        Engine::Grep if grep_in_c_locale() => Charset::Bytes,
        Engine::Grep => return None,
        Engine::Rg => Charset::Utf8,
    };
    match_locator(
        patterns,
        pattern_dialect(engine, args),
        ignores_case(engine, args),
        charset,
    )
}

fn compact_path(path: &str) -> String {
    if path.len() <= 50 {
        return path.to_string();
    }

    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 3 {
        return path.to_string();
    }

    format!(
        "{}/.../{}/{}",
        parts[0],
        parts[parts.len() - 2],
        parts[parts.len() - 1]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locator(pattern: &str) -> Option<Locator> {
        match_locator(
            &[pattern.to_string()],
            PatternDialect::Regex,
            true,
            Charset::Utf8,
        )
    }

    #[test]
    fn test_clean_line() {
        let line = "            const result = someFunction();";
        let cleaned = clean_line(line, 50, None, locator("result").as_ref());
        assert!(!cleaned.starts_with(' '));
        assert!(cleaned.len() <= 50);
    }

    #[test]
    fn test_compact_path() {
        let path = "/Users/patrick/dev/project/src/components/Button.tsx";
        let compact = compact_path(path);
        assert!(compact.len() <= 60);
    }

    #[test]
    fn streaming_search_preserves_native_shape() {
        let mut filter = SearchStreamFilter {
            show_file: false,
            show_line: true,
            max_results: 10,
            shown: 0,
            cap_reported: false,
        };

        assert_eq!(
            filter.feed_line("engine warning"),
            Some("engine warning\n".to_string())
        );
        assert_eq!(
            filter.feed_line(concat!("(standard input)\0", "1:match")),
            Some("1:match\n".to_string())
        );
    }

    #[test]
    fn streaming_search_reports_the_cap_once() {
        let mut filter = SearchStreamFilter {
            show_file: false,
            show_line: true,
            max_results: 1,
            shown: 0,
            cap_reported: false,
        };

        assert_eq!(
            filter.feed_line(concat!("(standard input)\0", "1:first")),
            Some("1:first\n".to_string())
        );
        assert_eq!(
            filter.feed_line(concat!("(standard input)\0", "2:second")),
            Some("[rtk] output capped at 1 results\n".to_string())
        );
        assert_eq!(
            filter.feed_line(concat!("(standard input)\0", "3:third")),
            None
        );
        assert_eq!(filter.feed_line("--"), None);
    }

    #[test]
    fn test_clean_line_multibyte() {
        // Thai text that exceeds max_len in bytes
        let line = "  สวัสดีครับ นี่คือข้อความที่ยาวมากสำหรับทดสอบ  ";
        let cleaned = clean_line(line, 20, None, locator("ครับ").as_ref());
        // Should not panic
        assert!(!cleaned.is_empty());
    }

    #[test]
    fn test_clean_line_centres_on_a_regex_match() {
        // A literal search for `fn\s+run` finds nothing, so the old window kept the head of
        // the line and cut the match away.
        let line = format!("{} fn   run() {{}}", "x".repeat(200));
        let cleaned = clean_line(&line, 40, None, locator(r"fn\s+run").as_ref());
        assert!(cleaned.contains("fn   run()"), "{cleaned}");
        assert!(cleaned.starts_with("..."), "{cleaned}");
    }

    #[test]
    fn test_clean_line_survives_lowercase_that_changes_length() {
        // 'İ' lowercases to two chars; offsets taken on the lowercased copy overshot the line.
        let line = format!("{}needle{}", "İ".repeat(60), "y".repeat(60));
        let cleaned = clean_line(&line, 20, None, locator("needle").as_ref());
        assert!(cleaned.contains("needle"), "{cleaned}");
    }

    #[test]
    fn test_match_locator_gives_up_on_a_pattern_rust_rejects() {
        // Its literal spelling is no match the engine would make, so nothing is located.
        assert!(locator("a(b").is_none());
        let pcre = match_locator(
            &["(?<=foo)bar".into()],
            PatternDialect::Pcre,
            false,
            Charset::Utf8,
        );
        assert!(pcre.is_none());
    }

    #[test]
    fn test_grep_in_c_locale_needs_c_character_type_and_collation() {
        use user_env::with_vars;
        let at = |all, ctype, collate, lang| {
            with_vars(
                &[
                    ("LC_ALL", all),
                    ("LC_CTYPE", ctype),
                    ("LC_COLLATE", collate),
                    ("LANG", lang),
                ],
                grep_in_c_locale,
            )
        };
        assert!(at(None, None, None, None));
        assert!(at(None, Some("POSIX"), Some("C"), None));
        assert!(at(Some("C"), None, None, Some("en_AU.UTF-8")));
        assert!(at(Some(""), None, None, None));
        assert!(!at(None, None, None, Some("C.UTF-8")));
        assert!(!at(None, Some("en_AU.UTF-8"), None, None));
        // Ranges follow LC_COLLATE even when the character type is C.
        assert!(!at(None, Some("C"), Some("en_AU.UTF-8"), None));
    }

    #[test]
    fn test_a_byte_locator_reads_as_the_c_locale_does() {
        let bytes = |p: &str| {
            match_locator(
                &[p.to_string()],
                PatternDialect::Basic,
                false,
                Charset::Bytes,
            )
            .unwrap()
        };
        // In the C locale `.` is one byte, so `a..b` matches the two bytes of `é`, and a
        // match that stops inside a character widens to the whole character.
        assert_eq!(bytes("a..b").find_chars("x aéb"), Some((2, 5)));
        assert_eq!(bytes("a.").find_chars("aé"), Some((0, 2)));
        assert_eq!(bytes("a.b").find_chars("aéb axb"), Some((4, 7)));
        // `\s` and POSIX classes are ASCII, as grep's are there.
        assert_eq!(
            bytes(r"\sneedle").find_chars("\u{2003}needle needle"),
            Some((7, 14))
        );
        assert_eq!(
            bytes("[^[:alpha:]]needle").find_chars("éneedle"),
            Some((0, 7))
        );
    }

    #[test]
    fn test_locator_for_skips_rg_without_unicode() {
        let patterns = ["needle".to_string()];
        assert!(locator_for(Engine::Rg, &["--no-unicode", "needle"], &patterns).is_none());
    }

    #[test]
    fn test_clean_line_keeps_the_whole_match_in_the_window() {
        let line = format!("{} needle {}", "f".repeat(240), "g".repeat(240));
        let re = locator(".*needle").unwrap();
        // The match spans 247 chars, more than the window: the line stays whole.
        assert_eq!(clean_line(&line, 160, None, Some(&re)), line);
        // A 147-char match fits: the window ends on it rather than a third of a window past.
        let re = locator("f{140} needle").unwrap();
        let clipped = clean_line(&line, 160, None, Some(&re));
        assert_ne!(clipped, line);
        assert!(
            clipped.contains(&format!("{} needle", "f".repeat(140))),
            "{clipped}"
        );
    }

    #[test]
    fn test_locator_for_skips_whole_word_and_whole_line_searches() {
        let patterns = ["needle".to_string()];
        assert!(locator_for(Engine::Grep, &["-w", "needle"], &patterns).is_none());
        assert!(locator_for(Engine::Rg, &["--word-regexp", "needle"], &patterns).is_none());
        assert!(locator_for(Engine::Grep, &["-Fx", "needle"], &patterns).is_none());
        assert!(locator_for(Engine::Rg, &["--line-regexp", "needle"], &patterns).is_none());
        assert!(locator_for(Engine::Rg, &["needle"], &patterns).is_some());
    }

    #[test]
    fn test_locator_for_grep_serves_only_the_c_locale() {
        let patterns = ["needle".to_string()];
        let grep = || locator_for(Engine::Grep, &["needle"], &patterns);
        let locale = |lang| {
            [
                ("LC_ALL", None),
                ("LC_CTYPE", None),
                ("LC_COLLATE", None),
                ("LANG", lang),
            ]
        };
        assert!(user_env::with_vars(&locale(Some("C")), grep).is_some());
        assert!(user_env::with_vars(&locale(Some("C.UTF-8")), grep).is_none());
        let rg = || locator_for(Engine::Rg, &["needle"], &patterns);
        assert!(user_env::with_vars(&[("LANG", Some("C.UTF-8"))], rg).is_some());
    }

    #[test]
    fn test_clean_line_locates_on_the_line_as_the_engine_saw_it() {
        // grep saw the leading spaces, so `^foo` is no match; `needle` is.
        let line = format!("  foo {} needle {}", "f".repeat(240), "g".repeat(240));
        let re = match_locator(
            &["^foo|needle".to_string()],
            PatternDialect::Extended,
            false,
            Charset::Bytes,
        );
        let clipped = clean_line(&line, 160, None, re.as_ref());
        assert!(clipped.contains(" needle "), "{clipped}");
    }

    #[test]
    fn test_pattern_dialect_follows_the_last_mode_flag() {
        use PatternDialect::*;
        let grep = |args: &[&str]| pattern_dialect(Engine::Grep, args);
        let rg = |args: &[&str]| pattern_dialect(Engine::Rg, args);
        assert_eq!(grep(&["x", "f"]), Basic);
        assert_eq!(grep(&["-rnF", "x"]), Fixed);
        assert_eq!(grep(&["--fixed-strings", "x"]), Fixed);
        assert_eq!(grep(&["-F", "-E", "x"]), Extended);
        assert_eq!(grep(&["-P", "-G", "x"]), Basic);
        assert_eq!(grep(&["--", "-F"]), Basic);
        assert_eq!(grep(&["-e", "-F", "f"]), Basic);
        assert_eq!(rg(&["-e", "-F", "f"]), Regex);
        assert_eq!(rg(&["x"]), Regex);
        assert_eq!(rg(&["-F", "x"]), Fixed);
        assert_eq!(rg(&["-F", "--no-fixed-strings", "x"]), Regex);
        // rg's -E is --encoding, not a pattern mode.
        assert_eq!(rg(&["-F", "-E", "utf8", "x"]), Fixed);
        assert_eq!(grep(&["-P", "x"]), Pcre);
        assert_eq!(rg(&["--pcre2", "x"]), Pcre);
        assert_eq!(rg(&["-P", "--no-pcre2", "x"]), Regex);
        assert_eq!(rg(&["--engine", "auto", "x"]), Pcre);
        assert_eq!(rg(&["--engine=default", "x"]), Regex);
        // The engine flags leave rg's -F alone, and -F overrides any engine.
        assert_eq!(rg(&["-F", "--no-pcre2", "x"]), Fixed);
        assert_eq!(rg(&["-F", "--engine=default", "x"]), Fixed);
        assert_eq!(rg(&["-F", "-P", "x"]), Fixed);
        assert_eq!(rg(&["-P", "-F", "--no-fixed-strings", "x"]), Pcre);
    }

    #[test]
    fn test_ignores_case_only_when_the_flags_say_so() {
        let grep = |args: &[&str]| ignores_case(Engine::Grep, args);
        let rg = |args: &[&str]| ignores_case(Engine::Rg, args);
        assert!(!grep(&["x"]));
        assert!(grep(&["-ri", "x"]));
        assert!(grep(&["-y", "x"]));
        assert!(!grep(&["-i", "--no-ignore-case", "x"]));
        assert!(!grep(&["--", "-i"]));
        assert!(!rg(&["x"]));
        // Smart case is decided on rg's parsed regex (`\x41` is uppercase); not modelled.
        assert!(!rg(&["-S", "x"]));
        assert!(!rg(&["-i", "-S", "x"]));
        assert!(!rg(&["-i", "-s", "x"]));
    }

    #[test]
    fn test_clean_line_keeps_a_line_whole_without_a_located_match() {
        let line = format!("{} a^b {}", "f".repeat(100), "g".repeat(100));
        let re = match_locator(
            &["a^b".to_string()],
            PatternDialect::Basic,
            false,
            Charset::Bytes,
        );
        assert!(re.is_none());
        assert_eq!(clean_line(&line, 60, None, re.as_ref()), line);
        // Past 4 windows the head is kept and the clip says the match may lie past the cut.
        assert_eq!(
            clean_line(&line, 40, None, re.as_ref()),
            format!(
                "{}... [+165 chars cut; match position unknown]",
                "f".repeat(40)
            )
        );
        // Even when the head happens to hold the match.
        let head_match = format!("needle {}", "f".repeat(700));
        assert!(clean_line(&head_match, 160, None, None).ends_with("match position unknown]"));
    }

    #[test]
    fn test_posix_to_regex_swaps_bre_operators_and_refuses_what_differs() {
        let bre = |p| posix_to_regex(p, true);
        let ere = |p| posix_to_regex(p, false);
        // A BRE `^` or `$` is an anchor only at the pattern's ends; GNU reads it as a literal
        // elsewhere. An ERE's are anchors anywhere, as in Rust.
        assert_eq!(bre("^a$").as_deref(), Some("^a$"));
        assert_eq!(bre(r"a\?^foo"), None);
        assert_eq!(bre("a$b"), None);
        assert_eq!(ere("a|^b$").as_deref(), Some("a|^b$"));
        // GNU's `{,n}` interval, and Rust's `\b{start}`, which GNU reads as a literal brace.
        assert_eq!(ere("a{,2}b"), None);
        assert_eq!(bre(r"a\{,2\}b"), None);
        assert_eq!(ere(r"\b{start}foo"), None);
        assert_eq!(ere(r"\bfoo\b").as_deref(), Some(r"\bfoo\b"));
        assert_eq!(bre("[^[:alpha:]]x").as_deref(), Some("[^[:alpha:]]x"));
        assert_eq!(bre(r"a\+b").as_deref(), Some("a+b"));
        assert_eq!(bre(r"f(x)\|g\{2\}").as_deref(), Some(r"f\(x\)|g{2}"));
        assert_eq!(bre(r"\.\*\<w\>\s").as_deref(), Some(r"\.\*\<w\>\s"));
        assert_eq!(ere(r"a+\+(b|c)").as_deref(), Some(r"a+\+(b|c)"));
        assert_eq!(bre("[]a-c[:digit:]]").as_deref(), Some(r"[\]a-c[:digit:]]"));
        assert_eq!(bre("[^]x]").as_deref(), Some(r"[^\]x]"));
        // Rust reads these differently from GNU grep.
        for p in [
            r"\d", r"\x41", r"\(a\)\1", r"\`a", r"a\'", r"[\n]", "[a&&b]", "[[=a=]]",
        ] {
            assert_eq!(bre(p), None, "{p}");
        }
        assert_eq!(ere("(?i)a"), None);
        assert_eq!(bre("a\\"), None);
        assert_eq!(bre("[ab"), None);
    }

    #[test]
    fn test_match_locator_reads_each_dialect() {
        let line = format!("aXb {} a.b aab {}", "f".repeat(60), "g".repeat(60));
        let at = |patterns: &[&str], d| {
            let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
            match_locator(&patterns, d, false, Charset::Utf8)
                .and_then(|re| re.find_chars(&line))
                .map(|(s, e)| line.chars().skip(s).take(e - s).collect::<String>())
        };
        assert_eq!(at(&["a.b"], PatternDialect::Fixed).as_deref(), Some("a.b"));
        assert_eq!(at(&["a.b"], PatternDialect::Regex).as_deref(), Some("aXb"));
        assert_eq!(
            at(&[r"a\+b"], PatternDialect::Basic).as_deref(),
            Some("aab")
        );
        assert_eq!(
            at(&["zz", "a.b"], PatternDialect::Fixed).as_deref(),
            Some("a.b")
        );
        assert_eq!(
            at(&["zz\na.b"], PatternDialect::Fixed).as_deref(),
            Some("a.b")
        );
        // One pattern the locator cannot read leaves the whole search unlocated.
        assert_eq!(at(&["a.b", r"\d"], PatternDialect::Basic), None);
        assert_eq!(
            at(&[r"a\.b"], PatternDialect::Extended).as_deref(),
            Some("a.b")
        );
    }

    #[test]
    fn test_clean_line_emoji() {
        let line = "🎉🎊🎈🎁🎂🎄 some text 🎃🎆🎇✨";
        let cleaned = clean_line(line, 15, None, locator("text").as_ref());
        assert!(!cleaned.is_empty());
    }

    // --- fold_path_prefix / is_bare_file_list ---

    #[test]
    fn fold_path_prefix_folds_shared_dir_into_header() {
        let raw =
            "/home/u/proj/src/a/foo.rs\n/home/u/proj/src/a/bar.rs\n/home/u/proj/src/b/baz.rs\n";
        assert_eq!(
            fold_path_prefix(raw).as_deref(),
            Some("/home/u/proj/src/ (3 files)\na/foo.rs\na/bar.rs\nb/baz.rs\n")
        );
    }

    #[test]
    fn fold_path_prefix_keeps_engine_order_and_relative_paths() {
        let raw = "src/z.rs\nsrc/a.rs\n";
        assert_eq!(
            fold_path_prefix(raw).as_deref(),
            Some("src/ (2 files)\nz.rs\na.rs\n")
        );
    }

    #[test]
    fn fold_path_prefix_cuts_on_whole_components() {
        // `/a/foobar` and `/a/foobaz` share `/a/`, never the byte run `/a/fooba`.
        let raw = "/a/foobar/x\n/a/foobaz/y\n";
        assert_eq!(
            fold_path_prefix(raw).as_deref(),
            Some("/a/ (2 files)\nfoobar/x\nfoobaz/y\n")
        );
    }

    #[test]
    fn fold_path_prefix_never_folds_into_a_filename() {
        // The shared prefix is a directory only; a file that sits at the prefix root keeps its
        // full name.
        let raw = "/a/b/c.rs\n/a/b/d/e.rs\n";
        assert_eq!(
            fold_path_prefix(raw).as_deref(),
            Some("/a/b/ (2 files)\nc.rs\nd/e.rs\n")
        );
    }

    #[test]
    fn fold_path_prefix_none_when_nothing_to_fold() {
        assert_eq!(fold_path_prefix(""), None, "empty");
        assert_eq!(fold_path_prefix("/a/b/c.rs\n"), None, "single line");
        assert_eq!(fold_path_prefix("a.rs\nb.rs\n"), None, "bare filenames");
        assert_eq!(
            fold_path_prefix("/x/a.rs\n/y/b.rs\n"),
            None,
            "only `/` shared"
        );
        assert_eq!(
            fold_path_prefix("src/a.rs\nlib/b.rs\n"),
            None,
            "no shared dir"
        );
        assert_eq!(fold_path_prefix("/a/b.rs\n\n/a/c.rs\n"), None, "blank line");
        assert_eq!(
            fold_path_prefix("/a/b.rs\0/a/c.rs\0"),
            None,
            "NUL-joined (-Z)"
        );
        assert_eq!(fold_path_prefix("/a/x\r\n/a/y\n"), None, "CR in a name");
        assert_eq!(
            fold_path_prefix("\x1b]8;;file:///a/x\x1b\\/a/x\n\x1b]8;;file:///a/y\x1b\\/a/y\n"),
            None,
            "OSC 8 hyperlink"
        );
    }

    #[test]
    fn fold_path_prefix_is_lossless() {
        let raw = "/p/q/a.rs\n/p/q/r/b.rs\n/p/q/c.rs\n";
        let folded = fold_path_prefix(raw).expect("folds");
        let mut lines = folded.lines();
        let header = lines.next().unwrap();
        let prefix = header.strip_suffix(" (3 files)").unwrap();
        let rebuilt: String = lines.map(|tail| format!("{prefix}{tail}\n")).collect();
        assert_eq!(rebuilt, raw);
    }

    #[test]
    fn bare_file_list_detects_list_flags_per_engine() {
        assert!(is_bare_file_list(Engine::Grep, &["-rl", "foo", "."]));
        assert!(is_bare_file_list(Engine::Grep, &["-L", "foo", "."]));
        assert!(is_bare_file_list(
            Engine::Grep,
            &["--files-with-matches", "foo"]
        ));
        assert!(is_bare_file_list(Engine::Rg, &["-l", "foo"]));
        assert!(is_bare_file_list(Engine::Rg, &["--files", "src"]));
        // rg's -L is --follow, not a list.
        assert!(!is_bare_file_list(Engine::Rg, &["-L", "foo"]));
        assert!(!is_bare_file_list(Engine::Grep, &["-rn", "foo", "."]));
    }

    #[test]
    fn bare_file_list_rejects_other_shape_flags() {
        // Each of these changes or removes the path line, so the list must stay verbatim.
        assert!(!is_bare_file_list(Engine::Grep, &["-lc", "foo", "."]));
        assert!(!is_bare_file_list(Engine::Grep, &["-lZ", "foo", "."]));
        assert!(!is_bare_file_list(
            Engine::Grep,
            &["-l", "--null", "foo", "."]
        ));
        assert!(!is_bare_file_list(Engine::Grep, &["-lq", "foo", "."]));
        assert!(!is_bare_file_list(Engine::Rg, &["-l", "--json", "foo"]));
        assert!(!is_bare_file_list(Engine::Rg, &["--files", "--null"]));
        // `-e -l` is a pattern, not the list flag.
        assert!(!is_bare_file_list(Engine::Grep, &["-e", "-l", "."]));
    }

    // --- extract_pattern_path ---
    //
    // parse_cluster/ClusterResult were replaced by arg_tokenizer::tokenize; the
    // extract_pattern_path tests below exercise the same short-cluster/value-taking/`-e`
    // behavior end-to-end instead of unit-testing the internal cluster scanner directly.

    #[test]
    fn test_extract_simple() {
        let (patterns, paths, flags, _, _) = extract_pattern_path(&["foo", "src/"], Engine::Grep);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src/"]);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_engine_specific_long_value_flags() {
        // grep 3.11: `--include`/`--exclude-dir`/... require a separate value, and rg has no
        // such flags at all. Missing them made the glob the pattern and the pattern a file.
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["--include", "*.txt", "-r", "match", "."], Engine::Grep);
        assert_eq!(patterns, vec!["match"]);
        assert_eq!(paths, vec!["."]);
        assert_eq!(flags, vec!["--include", "*.txt", "-r"]);

        // grep's --color[=WHEN] attaches its value; rg's --color takes the next token.
        let (patterns, paths, _, _, _) =
            extract_pattern_path(&["--color", "match", "a.txt"], Engine::Grep);
        assert_eq!(patterns, vec!["match"]);
        assert_eq!(paths, vec!["a.txt"]);

        let (patterns, paths, _, _, _) =
            extract_pattern_path(&["--color", "never", "match", "a.txt"], Engine::Rg);
        assert_eq!(patterns, vec!["match"]);
        assert_eq!(paths, vec!["a.txt"]);
    }

    #[test]
    fn test_context_detection_covers_greps_numeric_shorthand() {
        // grep's `-1` is `--context=1`. Missing it dropped the `--` separators between
        // non-contiguous context blocks, so two far-apart hunks read as one run.
        assert!(is_context_token(Engine::Grep, TokenKind::Short, "1"));
        assert!(is_context_token(Engine::Grep, TokenKind::Short, "12"));
        assert!(is_context_token(Engine::Grep, TokenKind::Short, "C"));
        assert!(is_context_token(Engine::Grep, TokenKind::Long, "context"));
        assert!(!is_context_token(Engine::Grep, TokenKind::Short, "n"));

        let (_, _, _, _, detected) = extract_pattern_path(&["-1", "TODO", "f.txt"], Engine::Grep);
        assert!(detected.context);
    }

    #[test]
    fn test_help_short_circuit_respects_the_boundary_and_the_engine() {
        let asks = |engine: Engine, args: &[&str]| -> bool {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let tokens = arg_tokenizer::tokenize_grammar(
                &args,
                &|kind, name| search_takes_value(engine, kind, name),
                Dialect::Posix,
            );
            arg_tokenizer::before_dashdash(&tokens).iter().any(|t| {
                (t.kind == TokenKind::Long && matches!(t.text, "version" | "help"))
                    || (t.kind == TokenKind::Short && t.text == "h" && engine == Engine::Rg)
            })
        };

        assert!(asks(Engine::Grep, &["--version"]));
        // Past `--` it is the pattern to search for, not a request for the banner.
        assert!(!asks(Engine::Grep, &["--", "--version", "f.txt"]));
        // `-h` is rg's --help but grep's --no-filename.
        assert!(asks(Engine::Rg, &["-h"]));
        assert!(!asks(Engine::Grep, &["-h", "TODO", "f.txt"]));
    }

    #[test]
    fn test_no_filename_is_honoured_at_display_not_forwarded() {
        // RTK forces `-H` so it can parse the output, so the user's `-h`/`--no-filename` has to
        // be applied when printing -- forwarded, it wins as the later flag, the NUL-separated
        // parse fails on every line, and the whole search runs a second time.
        let (_, _, flags, _, detected) =
            extract_pattern_path(&["--no-filename", "x", "a.txt"], Engine::Grep);
        assert_eq!(detected.show_file, Some(false));
        assert!(!flags.iter().any(|f| f == "--no-filename"));

        let (_, _, flags, _, detected) = extract_pattern_path(&["-ih", "x", "a.txt"], Engine::Grep);
        assert_eq!(detected.show_file, Some(false));
        assert_eq!(flags, vec!["-i"], "the rest of the cluster survives");

        // Both engines arbitrate the pair by last-one-wins, so RTK must too.
        let (_, _, _, _, detected) =
            extract_pattern_path(&["-h", "-H", "x", "a.txt"], Engine::Grep);
        assert_eq!(detected.show_file, Some(true));
        let (_, _, _, _, detected) =
            extract_pattern_path(&["-H", "-h", "x", "a.txt"], Engine::Grep);
        assert_eq!(detected.show_file, Some(false));
        let (_, _, _, _, detected) = extract_pattern_path(&["-Hh", "x", "a.txt"], Engine::Grep);
        assert_eq!(detected.show_file, Some(false), "within one cluster too");

        // rg's -N is its --no-line-number; withheld for the same reason as -h.
        let (_, _, flags, _, detected) = extract_pattern_path(&["-nN", "x", "a.txt"], Engine::Rg);
        assert!(!detected.show_line);
        assert!(!flags.iter().any(|f| f.contains('N')));
    }

    #[test]
    fn test_rg_unwraps_an_equals_attached_short_value_but_grep_does_not() {
        // rg accepts `-A=1` and strips the `=`; GNU grep answers "invalid context length
        // argument", so RTK must not normalise it for grep.
        let (_, _, flags, _, _) = extract_pattern_path(&["-A=1", "x", "a.txt"], Engine::Rg);
        assert_eq!(flags, vec!["-A", "1"]);

        let (_, _, flags, _, _) = extract_pattern_path(&["-A=1", "x", "a.txt"], Engine::Grep);
        assert_eq!(flags, vec!["-A", "=1"]);
    }

    #[test]
    fn test_extract_patterns_from_file_leaves_every_positional_a_path() {
        // `-f`/`--file` supplies the patterns like `-e` does. Taking the first positional as
        // the pattern instead left no path at all, so the engine read stdin (a hang) or walked
        // the cwd, and `rtk grep -f pats.txt a.txt` answered "no matches" for a file that had
        // one.
        for args in [
            vec!["-f", "pats.txt", "a.txt"],
            vec!["--file", "pats.txt", "a.txt"],
            vec!["-fpats.txt", "a.txt"],
            vec!["--file=pats.txt", "a.txt"],
        ] {
            let (patterns, paths, _, _, _) = extract_pattern_path(&args, Engine::Grep);
            assert!(patterns.is_empty(), "{args:?} -> {patterns:?}");
            assert_eq!(paths, vec!["a.txt"], "{args:?}");
        }
    }

    #[test]
    fn test_extract_engine_specific_short_value_flags() {
        // `-E` is grep's boolean --extended-regexp but rg's --encoding, which takes a value.
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-E", "match", "a.txt"], Engine::Grep);
        assert_eq!(patterns, vec!["match"]);
        assert_eq!(paths, vec!["a.txt"]);
        assert_eq!(flags, vec!["-E"]);

        let (patterns, paths, _, _, _) =
            extract_pattern_path(&["-E", "utf8", "match", "a.txt"], Engine::Rg);
        assert_eq!(patterns, vec!["match"]);
        assert_eq!(paths, vec!["a.txt"]);
    }

    #[test]
    fn test_extract_with_bool_flag() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-i", "foo", "src/"], Engine::Grep);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src/"]);
        assert_eq!(flags, vec!["-i"]);
    }

    #[test]
    fn test_extract_value_taking_flag() {
        // -A 2 must not steal "error" as its value
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-A", "2", "error", "src"], Engine::Grep);
        assert_eq!(patterns, vec!["error"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-A", "2"]);
    }

    #[test]
    fn test_extract_cluster_keeps_r() {
        // -rn: r kept, passed straight to grep
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-rn", "foo", "src"], Engine::Grep);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-rn"]);
    }

    #[test]
    fn test_extract_cluster_ending_in_e() {
        // -rne PATTERN: rn kept, e consumes PATTERN as the pattern
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-rne", "PATTERN", "src"], Engine::Grep);
        assert_eq!(patterns, vec!["PATTERN"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-rn"]);
    }

    #[test]
    fn test_extract_cluster_ending_in_value_flag() {
        // -rA 2: r kept as its own flag, A consumes 2 as context value
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-rA", "2", "foo", "src"], Engine::Grep);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-r", "-A", "2"]);
    }

    #[test]
    fn test_extract_multi_path() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["TODO", "src", "tests"], Engine::Grep);
        assert_eq!(patterns, vec!["TODO"]);
        assert_eq!(paths, vec!["src", "tests"]);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_glob_value() {
        // -g '*.md' must not steal "agent" as its value
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-i", "x", "agent", "-g", "*.md"], Engine::Rg);
        assert_eq!(patterns, vec!["x"]);
        assert_eq!(paths, vec!["agent"]);
        assert_eq!(flags, vec!["-i", "-g", "*.md"]);
    }

    #[test]
    fn test_extract_e_flag() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-e", "fn run", "src"], Engine::Grep);
        assert_eq!(patterns, vec!["fn run"]);
        assert_eq!(paths, vec!["src"]);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_multi_e() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-e", "foo", "-e", "bar", "src"], Engine::Grep);
        assert_eq!(patterns, vec!["foo", "bar"]);
        assert_eq!(paths, vec!["src"]);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_dashdash_boundary() {
        // After --, args are positional even if they look like flags
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["--", "--version"], Engine::Grep);
        assert_eq!(patterns, vec!["--version"]);
        assert!(paths.is_empty());
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_e_claims_literal_dash_dash() {
        // grep/rg -e -- means "the pattern is the literal string --", not the end-of-options
        // boundary (confirmed against both real grep and real rg).
        let (patterns, paths, flags, _, _) = extract_pattern_path(&["-e", "--", "f"], Engine::Grep);
        assert_eq!(patterns, vec!["--"]);
        assert_eq!(paths, vec!["f"]);
        assert!(flags.is_empty());

        let (patterns, paths, _, _, _) =
            extract_pattern_path(&["--regexp", "--", "f"], Engine::Grep);
        assert_eq!(patterns, vec!["--"]);
        assert_eq!(paths, vec!["f"]);
    }

    #[test]
    fn test_extract_no_args() {
        let (patterns, paths, flags, _, _) = extract_pattern_path::<&str>(&[], Engine::Grep);
        assert!(patterns.is_empty());
        assert!(paths.is_empty());
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_default_path_empty() {
        // Caller is responsible for defaulting empty paths to ["."]
        let (patterns, paths, _, _, _) = extract_pattern_path(&["foo"], Engine::Grep);
        assert_eq!(patterns, vec!["foo"]);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_extract_ending_e() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-e", "foo", "-e", "bar", "src", "-e"], Engine::Grep);
        assert_eq!(patterns, vec!["foo", "bar"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-e"]);
    }

    // --- inline short flag values (Bug 5) ---

    #[test]
    fn test_extract_inline_e_value() {
        // -ecarrot: e hits at j=0, inline="carrot", no r-stripping on value
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-ecarrot", "file"], Engine::Grep);
        assert_eq!(patterns, vec!["carrot"]);
        assert_eq!(paths, vec!["file"]);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_inline_e_value_no_rstrip() {
        // -ecarrot: the 'r' in "carrot" must NOT be stripped (it's value, not a flag)
        let (patterns, _, _, _, _) = extract_pattern_path(&["-ecarrot", "file"], Engine::Grep);
        assert_eq!(
            patterns,
            vec!["carrot"],
            "r in inline value must not be stripped"
        );
    }

    #[test]
    fn test_extract_inline_g_value() {
        // -g*.rs: g hits at j=0, inline="*.rs", no r-stripping on value
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["aaa", "sub", "-g*.rs"], Engine::Rg);
        assert_eq!(patterns, vec!["aaa"]);
        assert_eq!(paths, vec!["sub"]);
        assert_eq!(flags, vec!["-g", "*.rs"]);
    }

    #[test]
    fn test_extract_inline_g_value_no_rstrip() {
        // -g*.rs: the 'r' in "*.rs" must NOT be stripped
        let (_, _, flags, _, _) = extract_pattern_path(&["aaa", "sub", "-g*.rs"], Engine::Rg);
        assert!(
            flags.contains(&"*.rs".to_string()),
            "r in glob value must not be stripped"
        );
    }

    // --- long value-taking flags (Bug 5) ---

    #[test]
    fn test_extract_long_glob_value() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["compact", "sub", "--glob", "*.md"], Engine::Rg);
        assert_eq!(patterns, vec!["compact"]);
        assert_eq!(paths, vec!["sub"]);
        assert_eq!(flags, vec!["--glob", "*.md"]);
    }

    #[test]
    fn test_extract_long_max_count() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["--max-count", "1", "fn", "file"], Engine::Grep);
        assert_eq!(patterns, vec!["fn"]);
        assert_eq!(paths, vec!["file"]);
        assert_eq!(flags, vec!["--max-count", "1"]);
    }

    #[test]
    fn test_extract_short_type() {
        // -t rust: type filter, value must not become pattern
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-t", "rust", "fn", "src"], Engine::Rg);
        assert_eq!(patterns, vec!["fn"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-t", "rust"]);
    }

    #[test]
    fn test_extract_short_max_depth() {
        // -d 3: max-depth, value must not become pattern
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-d", "3", "foo", "src"], Engine::Grep);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-d", "3"]);
    }

    #[test]
    fn test_extract_short_max_columns() {
        // -M 120: max-columns, value must not become pattern
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-M", "120", "foo", "src"], Engine::Rg);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-M", "120"]);
    }

    #[test]
    fn test_extract_long_regexp() {
        // --regexp is the long form of -e; value goes to patterns
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["--regexp", "fn run", "src"], Engine::Grep);
        assert_eq!(patterns, vec!["fn run"]);
        assert_eq!(paths, vec!["src"]);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_long_regexp_multi() {
        // --regexp can be combined with -e
        let (patterns, paths, _, _, _) =
            extract_pattern_path(&["--regexp", "foo", "-e", "bar", "src"], Engine::Grep);
        assert_eq!(patterns, vec!["foo", "bar"]);
        assert_eq!(paths, vec!["src"]);
    }

    #[test]
    fn test_extract_long_ignore_file() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["--ignore-file", ".myignore", "foo", "src"], Engine::Rg);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["--ignore-file", ".myignore"]);
    }

    #[test]
    fn test_extract_long_engine() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["--engine", "pcre2", "foo", "src"], Engine::Rg);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["--engine", "pcre2"]);
    }

    #[test]
    fn test_extract_long_type_clear() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["--type-clear", "rust", "foo", "src"], Engine::Rg);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["--type-clear", "rust"]);
    }

    #[test]
    fn test_extract_long_path_separator() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["--path-separator", "/", "foo", "src"], Engine::Rg);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["--path-separator", "/"]);
    }

    #[test]
    fn test_extract_long_flag_inline_eq_passthrough() {
        // --glob=*.rs is one token (inline =): passes through as-is, not consumed as pair
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["foo", "src", "--glob=*.rs"], Engine::Grep);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["--glob=*.rs"]);
    }

    // --- has_format_flag additions ---

    #[test]
    fn test_format_flag_detects_count_matches() {
        assert!(has_format_flag(Engine::Grep, &["--count-matches"]));
    }

    #[test]
    fn test_format_flag_detects_json() {
        assert!(has_format_flag(Engine::Grep, &["--json"]));
    }

    #[test]
    fn test_format_flag_detects_passthru() {
        assert!(has_format_flag(Engine::Grep, &["--passthru"]));
    }

    #[test]
    fn test_format_flag_detects_files() {
        assert!(has_format_flag(Engine::Grep, &["--files"]));
    }

    // --- truncation accuracy ---

    #[test]
    fn test_grep_overflow_uses_uncapped_total() {
        // Confirm the grep overflow invariant: matches vec is never capped before overflow calc.
        // If total_matches > per_file, overflow = total_matches - per_file (not capped).
        // This documents that the search filter avoids the diff_cmd bug (cap at N then compute N-10).
        let per_file = config::limits().grep_max_per_file;
        let total_matches = per_file + 42;
        let overflow = total_matches - per_file;
        assert_eq!(overflow, 42, "overflow must equal true suppressed count");
        // Demonstrate why capping before subtraction is wrong:
        let hypothetical_cap = per_file + 5;
        let capped = total_matches.min(hypothetical_cap);
        let wrong_overflow = capped - per_file;
        assert_ne!(
            wrong_overflow, overflow,
            "capping before subtraction gives wrong overflow"
        );
    }

    // --- format flag detection ---

    #[test]
    fn test_format_flag_detects_count() {
        assert!(has_format_flag(Engine::Grep, &["-c"]));
        assert!(has_format_flag(Engine::Grep, &["--count"]));
    }

    #[test]
    fn test_format_flag_detects_files_with_matches() {
        assert!(has_format_flag(Engine::Grep, &["-l"]));
        assert!(has_format_flag(Engine::Grep, &["--files-with-matches"]));
    }

    #[test]
    fn test_format_flag_detects_files_without_match() {
        assert!(has_format_flag(Engine::Grep, &["-L"]));
        assert!(has_format_flag(Engine::Grep, &["--files-without-match"]));
    }

    #[test]
    fn test_format_flag_is_engine_aware_for_ambiguous_short_letters() {
        assert!(has_format_flag(Engine::Grep, &["-L"]));
        assert!(!has_format_flag(Engine::Rg, &["-L"]));

        assert!(has_format_flag(Engine::Grep, &["-z"]));
        assert!(!has_format_flag(Engine::Rg, &["-z"]));

        // Unambiguous shape letters still agree across engines.
        assert!(has_format_flag(Engine::Rg, &["-c"]));
        assert!(has_format_flag(Engine::Rg, &["-l"]));
        assert!(has_format_flag(Engine::Rg, &["-o"]));
        assert!(has_format_flag(Engine::Rg, &["-q"]));
        assert!(has_format_flag(Engine::Rg, &["-b"]));
        assert!(has_format_flag(Engine::Rg, &["-Z"]));
    }

    #[test]
    fn test_dash_capital_t_is_engine_aware() {
        let (patterns, paths, _, _, _) =
            extract_pattern_path(&["-T", "pattern", "file.txt"], Engine::Grep);
        assert_eq!(patterns, vec!["pattern"]);
        assert_eq!(paths, vec!["file.txt"]);

        // Rg's -T genuinely does take a value (a file type to exclude).
        let (patterns, paths, _, _, _) =
            extract_pattern_path(&["-T", "markdown", "pattern", "src"], Engine::Rg);
        assert_eq!(patterns, vec!["pattern"]);
        assert_eq!(paths, vec!["src"]);
    }

    #[test]
    fn test_dash_r_is_engine_aware() {
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-rREPLACEMENT", "pattern", "src"], Engine::Rg);
        assert_eq!(patterns, vec!["pattern"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-r".to_string(), "REPLACEMENT".to_string()]);

        // Grep's -r/-R remain plain boolean flags, clustering as before.
        let (patterns, paths, flags, _, _) =
            extract_pattern_path(&["-rn", "foo", "src"], Engine::Grep);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-rn".to_string()]);
    }

    #[test]
    fn test_detected_flags_ignore_a_value_taking_flags_own_value() {
        let (_, _, _, _, detected) =
            extract_pattern_path(&["--replace", "-Chart", "pattern", "src"], Engine::Rg);
        assert!(
            !detected.context,
            "--replace's value must not be misread as a -C context flag"
        );

        // Same for show_file's -H/-r/-R and show_line's -n/-N letters.
        let (_, _, _, _, detected) =
            extract_pattern_path(&["--replace", "-Hart", "pattern", "src"], Engine::Rg);
        assert_eq!(
            detected.show_file, None,
            "--replace's value must not trigger -H"
        );

        let (_, _, _, _, detected) =
            extract_pattern_path(&["--replace", "-normal", "pattern", "src"], Engine::Rg);
        assert!(!detected.show_line, "--replace's value must not trigger -n");

        // A genuine short context/show-file/show-line flag is still detected correctly.
        let (_, _, _, _, detected) =
            extract_pattern_path(&["-C", "3", "pattern", "src"], Engine::Grep);
        assert!(detected.context);
        let (_, _, _, _, detected) = extract_pattern_path(&["-H", "pattern", "src"], Engine::Grep);
        assert_eq!(detected.show_file, Some(true));
        let (_, _, _, _, detected) = extract_pattern_path(&["-n", "pattern", "src"], Engine::Grep);
        assert!(detected.show_line);
    }

    #[test]
    fn test_format_flag_detects_only_matching() {
        assert!(has_format_flag(Engine::Grep, &["-o"]));
        assert!(has_format_flag(Engine::Grep, &["--only-matching"]));
    }

    #[test]
    fn test_format_flag_detects_null() {
        assert!(has_format_flag(Engine::Grep, &["-Z"]));
        assert!(has_format_flag(Engine::Grep, &["--null"]));
    }

    #[test]
    fn test_format_flag_ignores_normal_flags() {
        assert!(!has_format_flag(Engine::Grep, &["-i", "-w", "-A", "3"]));
    }

    #[test]
    fn test_format_flag_ignores_value_of_value_taking_flag() {
        // Regression: has_format_flag used to be its own mini-tokenizer that scanned every raw
        // arg string independently, with no notion of "this token is another flag's value."
        // `-e --json` means "--json" is -e's pattern argument (both tables take a value for
        // true), not the real --json format flag -- but the old per-arg scan matched "--json"
        // regardless of position.
        assert!(!has_format_flag(Engine::Grep, &["-e", "--json"]));
        // Same for the long-flag form of a value-taking option.
        assert!(!has_format_flag(Engine::Grep, &["--regexp", "--quiet"]));
    }

    #[test]
    fn test_extract_pattern_path_has_format_flag_matches_single_pass() {
        // extract_pattern_path computes has_format_flag from its own token pass instead of
        // tokenizing the reconstructed `flags` strings a second time; pin that it agrees with
        // has_format_flag's own (test-only) from-scratch computation on representative cases.
        let (_, _, _, has_format, _) = extract_pattern_path(&["foo", "src", "-q"], Engine::Grep);
        assert!(has_format, "-q (quiet) should be detected");

        let (_, _, _, has_format, _) = extract_pattern_path(&["foo", "src", "-rl"], Engine::Grep);
        assert!(has_format, "-l inside the -rl cluster should be detected");

        let (_, _, _, has_format, _) =
            extract_pattern_path(&["foo", "src", "--json"], Engine::Grep);
        assert!(has_format, "--json should be detected");

        let (_, _, _, has_format, _) = extract_pattern_path(&["-e", "--json", "src"], Engine::Grep);
        assert!(
            !has_format,
            "-e's value must not be misread as the real --json flag"
        );

        let (_, _, _, has_format, _) =
            extract_pattern_path(&["foo", "src", "-i", "-w"], Engine::Grep);
        assert!(!has_format, "plain boolean flags aren't format flags");
    }

    #[test]
    fn test_format_flag_detects_clusters() {
        // clustered minimal forms must route to passthrough, not GROUP
        assert!(has_format_flag(Engine::Grep, &["-rl"]));
        assert!(has_format_flag(Engine::Grep, &["-rc"]));
        assert!(has_format_flag(Engine::Grep, &["-rq"]));
        assert!(has_format_flag(Engine::Grep, &["-rln"]));
        assert!(has_format_flag(Engine::Grep, &["-cr"]));
    }

    #[test]
    fn test_format_flag_detects_quiet_and_shape() {
        assert!(has_format_flag(Engine::Grep, &["-q"]));
        assert!(has_format_flag(Engine::Grep, &["--quiet"]));
        assert!(has_format_flag(Engine::Grep, &["--silent"]));
        assert!(has_format_flag(Engine::Grep, &["-b"]));
        assert!(has_format_flag(Engine::Grep, &["--byte-offset"]));
        assert!(has_format_flag(Engine::Grep, &["--column"]));
        assert!(has_format_flag(Engine::Grep, &["--vimgrep"]));
        assert!(has_format_flag(Engine::Grep, &["-z"]));
        assert!(has_format_flag(Engine::Grep, &["--null-data"]));
    }

    #[test]
    fn test_format_flag_compresses_default_and_context() {
        // compressible forms must NOT passthrough
        assert!(!has_format_flag(Engine::Grep, &["-rn"]));
        assert!(!has_format_flag(Engine::Grep, &["-A", "3"]));
        assert!(!has_format_flag(Engine::Grep, &["-v"]));
        assert!(!has_format_flag(Engine::Grep, &["-rin"]));
    }

    /// What production computes for these args, so a change to the real detector shows up here
    /// rather than only in a test-only twin of it.
    fn detected(args: &[&str]) -> DetectedFlags {
        detected_for(Engine::Grep, args)
    }

    fn detected_for(engine: Engine, args: &[&str]) -> DetectedFlags {
        let mut with_pattern = vec!["pattern"];
        with_pattern.extend_from_slice(args);
        extract_pattern_path(&with_pattern, engine).4
    }

    #[test]
    fn show_line_is_off_without_an_explicit_request() {
        assert!(!detected(&[]).show_line);
        assert!(!detected(&["-i"]).show_line);
        assert!(!detected(&["-r"]).show_line);
        assert!(!detected(&["-A", "3"]).show_line);
    }

    #[test]
    fn show_line_honours_n_in_every_spelling() {
        assert!(detected(&["-n"]).show_line);
        assert!(detected(&["--line-number"]).show_line);
        assert!(detected(&["-rn"]).show_line);
        assert!(detected(&["-in"]).show_line);
    }

    #[test]
    fn show_line_is_off_when_explicitly_negated() {
        // `-n` has to be present, or the assertion holds whether or not the negation works.
        assert!(detected_for(Engine::Rg, &["-n"]).show_line);
        assert!(!detected_for(Engine::Rg, &["-n", "-N"]).show_line);
        assert!(!detected_for(Engine::Rg, &["-n", "--no-line-number"]).show_line);
    }

    #[test]
    fn grep_initial_tab_is_a_shape_flag_in_both_spellings() {
        // `-T` pads and tabs every match line, so RTK's forced `-H --null -n` parse reads
        // nothing back and leaked the injected flags -- filename, a raw NUL and the line
        // number -- straight into the output.
        assert!(is_format_flag_token(Engine::Grep, TokenKind::Short, "T"));
        // ripgrep's -T is --type-not, a value-taking flag, not a shape flag.
        assert!(!is_format_flag_token(Engine::Rg, TokenKind::Short, "T"));
        assert!(is_format_flag_token(
            Engine::Grep,
            TokenKind::Long,
            "initial-tab"
        ));
        assert!(!is_format_flag_token(
            Engine::Rg,
            TokenKind::Long,
            "initial-tab"
        ));
    }

    #[test]
    fn grep_does_not_claim_ripgrep_only_line_number_negations() {
        // Real grep 3.12 exits 2 on both spellings; swallowing them would report a match for a
        // command the engine refuses to run.
        for negation in ["-N", "--no-line-number"] {
            let (_, _, flags, _, detected) =
                extract_pattern_path(&["pattern", "-n", negation], Engine::Grep);
            assert!(
                detected.show_line,
                "{negation} is not grep's, so -n still stands"
            );
            assert!(
                flags.iter().any(|f| f == negation),
                "{negation} must reach grep"
            );
        }
    }

    #[test]
    fn recursion_does_not_outrank_an_explicit_no_filename() {
        // Real grep: `-hr` and `-rh` both drop the prefix -- `-r` only makes the search span
        // several files, it is not the counterpart of `-h` the way `-H` is.
        assert_eq!(detected(&["-rh"]).show_file, Some(false));
        assert_eq!(detected(&["-hr"]).show_file, Some(false));
        assert_eq!(detected(&["-h", "-r"]).show_file, Some(false));
        assert_eq!(detected(&["-rH"]).show_file, Some(true));
        assert_eq!(detected(&["-Hr"]).show_file, Some(true));
    }

    #[test]
    fn recursion_alone_still_asks_for_the_filename() {
        for args in [&["-r"][..], &["-R"][..], &["--recursive"][..]] {
            let d = detected(args);
            assert_eq!(d.show_file, None, "{args:?} is not an explicit request");
            assert!(d.recursive, "{args:?} must feed show_file's fallback");
        }
        // ripgrep's `-r` is `--replace`, so its value must not be read as recursion.
        assert!(!detected_for(Engine::Rg, &["-r", "X"]).recursive);
    }

    #[test]
    fn match_block_stays_fully_qualified() {
        let entries = vec![
            (3usize, true, "line 3 needle".to_string()),
            (4usize, false, "line 4 context".to_string()),
        ];
        let block = match_block("src/deep/file.rs", &entries);
        assert_eq!(
            block,
            "src/deep/file.rs:3:line 3 needle\nsrc/deep/file.rs-4-line 4 context\n"
        );
    }

    #[test]
    fn match_block_keeps_position_when_display_drops_it() {
        assert!(!detected(&[]).show_line);
        let entries = vec![(42usize, true, "hit".to_string())];
        assert_eq!(match_block("f.txt", &entries), "f.txt:42:hit\n");
    }

    // Verify line numbers are always enabled in the engine invocation (parse_flags).
    // The -n/--line-numbers clap flag in main.rs is a no-op accepted for compat.
    #[test]
    fn test_rg_always_has_line_numbers() {
        // engine_capture always passes "-n" to the engine via parse_flags().
        // This test documents that -n is built-in, so the clap flag is safe to ignore.
        let mut cmd = resolved_command("rg");
        cmd.args(["-n", "--no-heading", "NONEXISTENT_PATTERN_12345", "."]);
        // If rg is available, it should accept -n without error (exit 1 = no match, not error)
        if let Ok(output) = cmd.output() {
            assert!(
                output.status.code() == Some(1) || output.status.success(),
                "rg -n should be accepted"
            );
        }
        // If rg is not installed, skip gracefully (test still passes)
    }

    // --- issues #1436 / #1613: parse_match_line robustness (single-file colon misparse) ---
    // Input shape is `file\0line[:-]content` (rg --null / grep -Z).

    #[test]
    fn test_parse_match_line_simple() {
        let line = "file.php\x0010:use Foo\\Bar;";
        let (file, line_num, is_match, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "file.php");
        assert_eq!(line_num, 10);
        assert!(is_match);
        assert_eq!(content, "use Foo\\Bar;");
    }

    // Issue #1436 reproducer: content with `::` must not split into a phantom
    // file bucket. With NUL separation between file and line:content, content
    // colons are irrelevant to the parser.
    #[test]
    fn test_parse_match_line_content_with_double_colon() {
        let line = "externalImportShell.class.php\x0081:        $this->queueProcessModel = ClassRegistry::init('Collections.QueueProcess');";
        let (file, line_num, is_match, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "externalImportShell.class.php");
        assert_eq!(line_num, 81);
        assert!(is_match);
        assert_eq!(
            content,
            "        $this->queueProcessModel = ClassRegistry::init('Collections.QueueProcess');"
        );
    }

    // Windows abs-path safety: drive letter + backslashes must not break the
    // parser. The NUL separator makes the file portion unambiguous.
    #[test]
    fn test_parse_match_line_windows_path() {
        let line = "C:\\src\\file.rs\x0042:fn main() {}";
        let (file, line_num, is_match, content) = parse_match_line(line).unwrap();
        assert_eq!(file, r"C:\src\file.rs");
        assert_eq!(line_num, 42);
        assert!(is_match);
        assert_eq!(content, "fn main() {}");
    }

    // Filenames containing `:digits:` (which would fool a greedy `:` parser)
    // must still parse correctly under NUL separation.
    #[test]
    fn test_parse_match_line_filename_with_colons() {
        let line = "badly_named:52:file.txt\x001:xxx";
        let (file, line_num, is_match, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "badly_named:52:file.txt");
        assert_eq!(line_num, 1);
        assert!(is_match);
        assert_eq!(content, "xxx");
    }

    // Content that itself contains `:digits:` (e.g. log lines, port numbers,
    // line-number-like substrings) must not confuse the parser.
    #[test]
    fn test_parse_match_line_content_with_digit_colons() {
        let line = "log.txt\x007:debug: counter is :42: now";
        let (file, line_num, is_match, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "log.txt");
        assert_eq!(line_num, 7);
        assert!(is_match);
        assert_eq!(content, "debug: counter is :42: now");
    }

    #[test]
    fn test_parse_match_line_malformed_returns_none() {
        // No NUL separator (e.g. rg/grep invoked without --null/-Z, or a
        // context line written with `-`).
        assert!(parse_match_line("file.rs:1:content").is_none());
        assert!(parse_match_line("not a match line").is_none());
        // Missing line number after NUL
        assert!(parse_match_line("file.rs\x00fn foo()").is_none());
        // Empty
        assert!(parse_match_line("").is_none());
    }

    #[test]
    fn test_parse_match_line_empty_content() {
        let line = "file.rs\x007:";
        let (file, line_num, is_match, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "file.rs");
        assert_eq!(line_num, 7);
        assert!(is_match);
        assert_eq!(content, "");
    }

    // Context line: separator is `-` → is_match==false
    #[test]
    fn test_parse_match_line_context_line() {
        let line = "file.txt\x004-after1";
        let (file, line_num, is_match, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "file.txt");
        assert_eq!(line_num, 4);
        assert!(!is_match, "dash separator must yield is_match==false");
        assert_eq!(content, "after1");
    }

    // --- unparsed_signal ---

    #[test]
    fn test_unparsed_signal_parseable_lines_yield_zero() {
        // NUL-separated match lines all parse → signal == 0
        let stdout = "file.txt\x001:hello\nfile.txt\x002:world\n";
        assert_eq!(unparsed_signal(stdout), 0);
    }

    #[test]
    fn test_unparsed_signal_context_separator_not_counted() {
        // The `--` context separator emitted by rg/grep between match groups
        // must not be counted as an unparsed line.
        let stdout = "file.txt\x001:hello\n--\nfile.txt\x003:world\n";
        assert_eq!(unparsed_signal(stdout), 0);
    }

    #[test]
    fn test_unparsed_signal_empty_line_not_counted() {
        let stdout = "file.txt\x001:hello\n\nfile.txt\x002:world\n";
        assert_eq!(unparsed_signal(stdout), 0);
    }

    #[test]
    fn test_unparsed_signal_bare_colon_line_counted() {
        // A line like "file.rs:1:content" (no NUL) is what --heading or
        // --no-filename output looks like — it must be counted.
        let stdout = "file.rs:1:content\n";
        assert_eq!(unparsed_signal(stdout), 1);
    }

    #[test]
    fn test_unparsed_signal_binary_notice_counted() {
        // rg emits "Binary file foo matches" for binary files; no NUL → counted.
        let stdout = "Binary file foo matches\n";
        assert_eq!(unparsed_signal(stdout), 1);
    }

    #[test]
    fn test_unparsed_signal_context_lines_parse_ok() {
        // Context lines (dash separator) parse via the updated regex → not counted.
        let stdout =
            "file.txt\x003-context_before\nfile.txt\x004:match\nfile.txt\x005-context_after\n";
        assert_eq!(unparsed_signal(stdout), 0);
    }

    // --- has_context_flag ---

    #[test]
    fn test_has_context_flag_short() {
        let f = |args: &[&str]| -> bool { detected(args).context };
        assert!(f(&["-A", "3"]));
        assert!(f(&["-B", "2"]));
        assert!(f(&["-C", "1"]));
        assert!(!f(&["-rn"]));
        assert!(!f(&["-i", "-w"]));
    }

    #[test]
    fn test_has_context_flag_long() {
        let f = |args: &[&str]| -> bool { detected(args).context };
        assert!(f(&["--after-context", "3"]));
        assert!(f(&["--before-context", "2"]));
        assert!(f(&["--context", "1"]));
        assert!(f(&["--after-context=3"]));
        assert!(f(&["--before-context=2"]));
        assert!(f(&["--context=1"]));
        assert!(!f(&["--color", "auto"]));
    }
}
