//! OpenAI / Anthropic request-body rewriting.
//!
//! Only plain text is rewritten: `text` blocks, string contents and the text of
//! tool results. Tool calls, tool inputs, images, documents, thinking blocks,
//! `cache_control` markers and every other field pass through untouched.

use crate::compressor::{BlockKind, CompressorEngine};
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiFormat {
    /// `POST /v1/chat/completions`
    OpenAI,
    /// `POST /v1/messages`
    Anthropic,
}

pub struct Rewrite {
    /// The new body, or `None` if nothing changed (forward the original bytes).
    pub body: Option<Vec<u8>>,
    pub model: Option<String>,
    /// All prompt text as sent by the client, and as forwarded.
    pub text_before: String,
    pub text_after: String,
    pub elapsed: Duration,
}

/// A pointer to a text field inside the JSON document, plus how to treat it.
struct Slot {
    path: Vec<PathSeg>,
    kind: BlockKind,
    /// Assistant history is counted but never rewritten.
    frozen: bool,
}

#[derive(Clone)]
enum PathSeg {
    Key(&'static str),
    Idx(usize),
}

fn get_mut<'a>(v: &'a mut Value, path: &[PathSeg]) -> Option<&'a mut Value> {
    path.iter().try_fold(v, |cur, seg| match seg {
        PathSeg::Key(k) => cur.get_mut(*k),
        PathSeg::Idx(i) => cur.get_mut(*i),
    })
}

fn get<'a>(v: &'a Value, path: &[PathSeg]) -> Option<&'a Value> {
    path.iter().try_fold(v, |cur, seg| match seg {
        PathSeg::Key(k) => cur.get(*k),
        PathSeg::Idx(i) => cur.get(*i),
    })
}

fn with(path: &[PathSeg], seg: PathSeg) -> Vec<PathSeg> {
    let mut p = path.to_vec();
    p.push(seg);
    p
}

/// Collect text slots from a `content` value: a string, or an array of blocks.
fn content_slots(content: &Value, path: Vec<PathSeg>, kind: BlockKind, frozen: bool, out: &mut Vec<Slot>) {
    match content {
        Value::String(_) => out.push(Slot { path, kind, frozen }),
        Value::Array(blocks) => {
            for (i, b) in blocks.iter().enumerate() {
                let bp = with(&path, PathSeg::Idx(i));
                match b.get("type").and_then(Value::as_str) {
                    Some("text") if b.get("text").is_some_and(Value::is_string) => {
                        out.push(Slot { path: with(&bp, PathSeg::Key("text")), kind, frozen })
                    }
                    // Anthropic tool results nest their own content.
                    Some("tool_result") => {
                        if let Some(c) = b.get("content") {
                            content_slots(c, with(&bp, PathSeg::Key("content")), BlockKind::Tool, frozen, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// One group of slots per message: the focus is computed per message.
fn collect(v: &Value, format: ApiFormat, engine: &CompressorEngine) -> Vec<Vec<Slot>> {
    let mut groups = Vec::new();
    if format == ApiFormat::Anthropic {
        if let Some(sys) = v.get("system") {
            let mut g = Vec::new();
            content_slots(sys, vec![PathSeg::Key("system")], BlockKind::Human, false, &mut g);
            groups.push(g);
        }
    }
    let Some(msgs) = v.get("messages").and_then(Value::as_array) else { return groups };
    for (i, m) in msgs.iter().enumerate() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        let (kind, frozen) = match (format, role) {
            (_, "user" | "system" | "developer") => (BlockKind::Human, false),
            (ApiFormat::OpenAI, "tool" | "function") => (BlockKind::Tool, false),
            // Anthropic assistant turns may carry signed thinking; never touch them.
            (ApiFormat::Anthropic, "assistant") => (BlockKind::Assistant, true),
            (ApiFormat::OpenAI, "assistant") => (BlockKind::Assistant, engine.mode != crate::compressor::Mode::Max),
            _ => (BlockKind::Assistant, true),
        };
        let mut g = Vec::new();
        if let Some(c) = m.get("content") {
            content_slots(c, vec![PathSeg::Key("messages"), PathSeg::Idx(i), PathSeg::Key("content")], kind, frozen, &mut g);
        }
        groups.push(g);
    }
    groups
}

/// Rewrite a request body. Returns `None` when the body isn't JSON we understand.
pub fn rewrite(body: &[u8], format: ApiFormat, engine: &CompressorEngine) -> Option<Rewrite> {
    let start = Instant::now();
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let model = v.get("model").and_then(Value::as_str).map(String::from);
    let groups = collect(&v, format, engine);

    let mut before = String::new();
    let mut after = String::new();
    let mut changed = false;
    for group in groups {
        let texts: Vec<String> = group
            .iter()
            .map(|s| get(&v, &s.path).and_then(Value::as_str).unwrap_or("").to_string())
            .collect();
        let focus = engine.focus_for(texts.iter().map(String::as_str));
        for (slot, original) in group.iter().zip(&texts) {
            before.push_str(original);
            before.push('\n');
            if slot.frozen {
                after.push_str(original);
                after.push('\n');
                continue;
            }
            let compressed = engine.compress(original, slot.kind, &focus);
            after.push_str(&compressed);
            after.push('\n');
            if *compressed != **original {
                if let Some(target) = get_mut(&mut v, &slot.path) {
                    *target = Value::String(compressed.to_string());
                    changed = true;
                }
            }
        }
    }
    let body = if changed { serde_json::to_vec(&v).ok() } else { None };
    Some(Rewrite { body, model, text_before: before, text_after: after, elapsed: start.elapsed() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressor::Mode;
    use serde_json::json;

    fn log_dump() -> String {
        let mut s = String::new();
        for i in 0..200 {
            s.push_str(&format!("2026-09-23 10:00:{:02} INFO worker: processed batch {i}\n", i % 60));
        }
        s.push_str("2026-09-23 10:01:00 ERROR worker: batch 201 failed: connection refused\n");
        s
    }

    #[test]
    fn anthropic_tool_result_is_compressed_and_thinking_untouched() {
        let body = json!({
            "model": "claude-opus-5",
            "system": [{"type": "text", "text": "You are helpful.", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": "Why did the worker fail?"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "", "signature": "abc"},
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "cat   worker.log"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": log_dump()}
                ]}
            ]
        });
        let engine = CompressorEngine::new(Mode::Balanced);
        let rw = rewrite(&serde_json::to_vec(&body).unwrap(), ApiFormat::Anthropic, &engine).unwrap();
        let out: Value = serde_json::from_slice(&rw.body.unwrap()).unwrap();
        let tr = out["messages"][2]["content"][0]["content"].as_str().unwrap();
        assert!(tr.contains("batch 201 failed: connection refused"));
        assert!(tr.contains("200 INFO/DEBUG lines hidden"));
        assert_eq!(out["messages"][1], body["messages"][1]);
        assert_eq!(out["system"][0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(rw.model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn openai_parts_and_tool_messages() {
        let body = json!({
            "model": "gpt-5",
            "messages": [
                {"role": "system", "content": "Hello there! Could you please help me."},
                {"role": "user", "content": [{"type": "text", "text": "Thanks in advance!\n\n\n\nfix it"}, {"type": "image_url", "image_url": {"url": "x"}}]},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "c", "type": "function", "function": {"name": "run", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c", "content": log_dump()}
            ]
        });
        let engine = CompressorEngine::new(Mode::Balanced);
        let rw = rewrite(&serde_json::to_vec(&body).unwrap(), ApiFormat::OpenAI, &engine).unwrap();
        let out: Value = serde_json::from_slice(&rw.body.unwrap()).unwrap();
        assert_eq!(out["messages"][0]["content"], "help me.");
        assert_eq!(out["messages"][1]["content"][0]["text"], "\nfix it");
        assert_eq!(out["messages"][1]["content"][1], body["messages"][1]["content"][1]);
        assert_eq!(out["messages"][2], body["messages"][2]);
        assert!(out["messages"][3]["content"].as_str().unwrap().len() < 200);
    }

    #[test]
    fn unchanged_body_is_not_reserialized() {
        let body = br#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#;
        let engine = CompressorEngine::new(Mode::Safe);
        assert!(rewrite(body, ApiFormat::OpenAI, &engine).unwrap().body.is_none());
    }
}
