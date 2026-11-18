//! Fixture builders shared by the integration tests. Each builder returns one
//! JSON line without a trailing newline.
#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde_json::{Value, json};

pub const T0: &str = "2026-11-01T10:00:00.000Z";
pub const T0_SECS: i64 = 1793527200;
pub const T1: &str = "2026-11-01T10:05:00.000Z";
pub const T1_SECS: i64 = 1793527500;

pub fn claude_user(sid: &str, cwd: &str, ts: &str, text: &str) -> String {
    json!({
        "type": "user",
        "isSidechain": false,
        "message": {"role": "user", "content": text},
        "timestamp": ts,
        "cwd": cwd,
        "sessionId": sid,
    })
    .to_string()
}

pub fn claude_meta_user(sid: &str, ts: &str, text: &str) -> String {
    json!({
        "type": "user",
        "isSidechain": false,
        "isMeta": true,
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        "timestamp": ts,
        "sessionId": sid,
    })
    .to_string()
}

fn assistant(sid: &str, ts: &str, block: Value, sidechain: bool) -> String {
    json!({
        "type": "assistant",
        "isSidechain": sidechain,
        "message": {"role": "assistant", "content": [block]},
        "timestamp": ts,
        "sessionId": sid,
    })
    .to_string()
}

pub fn claude_text(sid: &str, ts: &str, text: &str) -> String {
    assistant(sid, ts, json!({"type": "text", "text": text}), false)
}

pub fn claude_thinking(sid: &str, ts: &str, text: &str) -> String {
    assistant(
        sid,
        ts,
        json!({"type": "thinking", "thinking": text}),
        false,
    )
}

pub fn claude_tool_use(sid: &str, ts: &str, name: &str, input: Value) -> String {
    assistant(
        sid,
        ts,
        json!({"type": "tool_use", "name": name, "input": input}),
        false,
    )
}

pub fn claude_tool_result(sid: &str, ts: &str, out: &str) -> String {
    json!({
        "type": "user",
        "isSidechain": false,
        "message": {"role": "user", "content": [{"type": "tool_result", "content": out}]},
        "timestamp": ts,
        "sessionId": sid,
    })
    .to_string()
}

pub fn claude_title(sid: &str, title: &str) -> String {
    json!({"type": "ai-title", "aiTitle": title, "sessionId": sid}).to_string()
}

pub fn claude_sidechain(sid: &str, ts: &str, text: &str) -> String {
    assistant(sid, ts, json!({"type": "text", "text": text}), true)
}

pub fn pi_header(id: &str, cwd: &str, ts: &str) -> String {
    json!({"type": "session", "id": id, "cwd": cwd, "timestamp": ts}).to_string()
}

fn pi_message(ts: &str, message: Value) -> String {
    json!({"type": "message", "timestamp": ts, "message": message}).to_string()
}

pub fn pi_user(ts: &str, text: &str) -> String {
    pi_message(
        ts,
        json!({"role": "user", "content": [{"type": "text", "text": text}]}),
    )
}

pub fn pi_assistant(ts: &str, text: &str) -> String {
    pi_message(
        ts,
        json!({"role": "assistant", "content": [{"type": "text", "text": text}]}),
    )
}

pub fn pi_system(ts: &str, text: &str) -> String {
    pi_message(
        ts,
        json!({"role": "system", "content": "", "sections": {"preamble": text}}),
    )
}

pub fn pi_thinking(ts: &str, text: &str) -> String {
    pi_message(
        ts,
        json!({"role": "assistant", "content": [{"type": "thinking", "thinking": text}]}),
    )
}

/// An assistant tool call, shaped as pi writes it:
/// `{"type":"toolCall","id":..,"name":..,"arguments":{..}}`.
pub fn pi_tool_call(ts: &str, name: &str, arguments: Value) -> String {
    pi_message(
        ts,
        json!({
            "role": "assistant",
            "content": [{"type": "toolCall", "id": "call_1", "name": name, "arguments": arguments}],
        }),
    )
}

pub fn pi_tool_result(ts: &str, tool_name: &str, out: &str) -> String {
    pi_message(
        ts,
        json!({
            "role": "toolResult",
            "toolCallId": "call_1",
            "toolName": tool_name,
            "content": [{"type": "text", "text": out}],
        }),
    )
}

/// Whether tests that read real data (`#[ignore]`d ones) may run: only with
/// `HOUND_LIVE_TESTS=1`.
pub fn live_tests_enabled() -> bool {
    std::env::var("HOUND_LIVE_TESTS").as_deref() == Ok("1")
}

/// Writes `lines` joined by `'\n'` with a trailing `'\n'`, creating parent directories.
pub fn write_lines(path: &Path, lines: &[String]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(path, body).unwrap();
}

/// Appends `raw` to `path` verbatim.
pub fn append_raw(path: &Path, raw: &str) {
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(raw.as_bytes()).unwrap();
}
