//! Property check for `rewrite_command`: a rewrite may change the words of a
//! command but never its shell structure. Every operator, pipe, redirect and
//! shellism the lexer sees in the original must appear, in order, in the
//! rewrite; otherwise a hook would run something other than what the agent
//! asked for.

use super::lexer::{TokenKind, tokenize};
use super::registry::rewrite_command;

/// Commands RTK rewrites, commands it leaves alone, and words that hold
/// operator characters inside quotes.
const COMMANDS: &[&str] = &[
    "git status",
    "git log --oneline -5",
    "git diff HEAD~1",
    "cargo test",
    "cargo build --release",
    "ls -la",
    "grep -rn foo src",
    "grep \"a && b\" notes.txt",
    "rg 'x | y' src",
    "cat README.md",
    "head -n 5 file.txt",
    "tail -20 app.log",
    "find . -name '*.rs'",
    "npm test",
    "pytest -q",
    "docker ps",
    "echo \"done; ok\"",
    "git commit -m \"fix: a || b\"",
    "FOO=1 cargo check",
    "rtk git status",
    "sleep 1",
    "wc -l src/main.rs",
];

const REDIRECTS: &[&str] = &[
    "",
    "",
    "",
    " 2>&1",
    " > out.txt",
    " 2>/dev/null",
    " >> log.txt",
    " < in.txt",
    " &> all.log",
];

/// `&!` and `&|` are one operator to zsh (and `&|` to fish) but two tokens to
/// the lexer; glued or spaced, a rewrite must keep both.
const OPERATORS: &[&str] = &[
    " && ", " || ", " ; ", " | ", " |& ", " & ", " &! ", " &!", " &| ", " &|",
];

/// The non-word tokens of `cmd`, in order.
fn structure(cmd: &str) -> Vec<(TokenKind, String)> {
    tokenize(cmd)
        .into_iter()
        .filter(|t| t.kind != TokenKind::Arg)
        .map(|t| (t.kind, t.value))
        .collect()
}

/// SplitMix64: a fixed seed makes every run check the same commands.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

fn segment(rng: &mut Rng) -> String {
    format!("{}{}", rng.pick(COMMANDS), rng.pick(REDIRECTS))
}

#[test]
fn rewrites_keep_every_operator_and_redirect() {
    let mut rng = Rng(0x5EED_2026_1103);
    let mut rewritten = 0;
    let mut failures = Vec::new();
    for _ in 0..20_000 {
        let mut cmd = segment(&mut rng);
        for _ in 0..rng.next() % 4 {
            cmd.push_str(rng.pick(OPERATORS));
            cmd.push_str(&segment(&mut rng));
        }
        let Some(out) = rewrite_command(&cmd, &[], &[]) else {
            continue;
        };
        if out != cmd {
            rewritten += 1;
        }
        if structure(&out) != structure(&cmd) {
            failures.push(format!("{cmd:?}\n  -> {out:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} rewrites changed the shell structure; first 10:\n{}",
        failures.len(),
        failures[..failures.len().min(10)].join("\n")
    );
    // Guard against a generator that stops reaching the rewriter.
    assert!(
        rewritten > 5_000,
        "only {rewritten} of 20000 commands were rewritten"
    );
}
