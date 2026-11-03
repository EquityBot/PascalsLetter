//! A rewrite may change a command's words but never its shell structure: every operator,
//! pipe, redirect and shellism the lexer sees must survive, in order. The property test in
//! src/discover/rewrite_properties.rs checks generated commands; this target lets libFuzzer
//! search arbitrary strings.
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

fuzz_target!(|cmd: &str| {
    if let Some(out) = rewrite_command(cmd, &[], &[]) {
        assert_eq!(structure(&out), structure(cmd), "{cmd:?} -> {out:?}");
    }
});
