//! A rewrite may change a command's words but never its shell structure: every operator,
//! pipe, redirect and shellism the lexer sees in the command bash parses must survive, in
//! order. The property test in src/discover/rewrite_properties.rs checks generated commands;
//! this target lets libFuzzer search arbitrary strings.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rtk::discover::lexer::{TokenKind, tokenize};
use rtk::discover::registry::rewrite_command;

fn structure(cmd: &str) -> Vec<(TokenKind, String)> {
    tokenize(cmd)
        .into_iter()
        .filter(|t| t.kind != TokenKind::Arg)
        .map(|t| (t.kind, t.value))
        .collect()
}

/// The command bash parses: it deletes every unescaped `\<NL>` (rtk also takes `\<CR><NL>`)
/// before splitting it into tokens, so `&\<NL>&` runs as `&&`. The lexer keeps them. This is
/// written apart from rtk's own helper so the target still checks that helper.
fn bash_joined(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut rest = cmd;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(tail) = after
            .strip_prefix('\n')
            .or_else(|| after.strip_prefix("\r\n"))
        {
            rest = tail;
        } else {
            // An escaped character, another backslash included, stays as written.
            let len = after.chars().next().map_or(0, char::len_utf8);
            out.push_str(&rest[i..=i + len]);
            rest = &after[len..];
        }
    }
    out.push_str(rest);
    out
}

fuzz_target!(|cmd: &str| {
    if let Some(out) = rewrite_command(cmd, &[], &[]) {
        assert_eq!(
            structure(&bash_joined(&out)),
            structure(&bash_joined(cmd)),
            "{cmd:?} -> {out:?}"
        );
    }
});
