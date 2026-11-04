//! Integration test: `rtk session` sets Bash against all tool output, the
//! denominator `rtk gain` lacks. Runs the binary over fixture logs, so it reads
//! the figure the user sees.

use serde_json::{Value, json};
use std::fs;
use std::path::Path;

mod common;

fn tool_call(id: &str, name: &str, input: Value) -> String {
    json!({"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": id, "name": name, "input": input}
    ]}})
    .to_string()
}

fn tool_result(id: &str, content: Value) -> String {
    json!({"type": "user", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": id, "content": content}
    ]}})
    .to_string()
}

fn write_log(claude_dir: &Path, relative_path: &str, lines: &[String]) {
    let path = claude_dir.join("projects/-proj").join(relative_path);
    fs::create_dir_all(path.parent().expect("log has a parent")).expect("create log folder");
    fs::write(path, lines.join("\n")).expect("write log");
}

fn rtk_session(claude_dir: &Path) -> String {
    let output = common::rtk_command()
        .arg("session")
        .env("CLAUDE_CONFIG_DIR", claude_dir)
        .output()
        .expect("spawn rtk session");
    assert!(
        output.status.success(),
        "rtk session failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn share_counts_every_tool_in_both_content_forms_and_sessions_without_bash() {
    let claude_dir = common::tempdir();

    // Read logs a string, an MCP tool an array of blocks. The image is not text.
    write_log(
        claude_dir.path(),
        "with-bash.jsonl",
        &[
            tool_call("b1", "Bash", json!({"command": "git status"})),
            tool_result("b1", json!("x".repeat(100))),
            tool_call("r1", "Read", json!({"file_path": "/tmp/foo"})),
            tool_result("r1", json!("x".repeat(300))),
            tool_call("m1", "mcp__docs__fetch", json!({})),
            tool_result(
                "m1",
                json!([
                    {"type": "text", "text": "x".repeat(200)},
                    {"type": "image", "source": {"data": "x".repeat(5000)}},
                ]),
            ),
        ],
    );
    // The table skips a session with no Bash, but its output still counts.
    write_log(
        claude_dir.path(),
        "no-bash.jsonl",
        &[
            tool_call("g1", "Grep", json!({"pattern": "TODO"})),
            tool_result("g1", json!("x".repeat(400))),
        ],
    );
    // Subagent logs are left out, as the table leaves them out.
    write_log(
        claude_dir.path(),
        "parent/subagents/agent-1.jsonl",
        &[
            tool_call("b2", "Bash", json!({"command": "git log"})),
            tool_result("b2", json!("x".repeat(9000))),
        ],
    );

    let stdout = rtk_session(claude_dir.path());

    assert!(
        stdout.contains("Bash share of tool output: 10% (100 of 1.0K bytes, 2 of 2 sessions)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Only Bash reaches RTK's default hook"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Images and subagent logs are not counted."),
        "{stdout}"
    );
}
