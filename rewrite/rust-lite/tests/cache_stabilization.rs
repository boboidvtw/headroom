//! M6 安全網：cache 穩定化 port 到 Rust（與 Python tests/test_cache_stabilization.py 對齊）。
//!
//!   1. tools 正規化：tool 順序 / schema key 順序怎麼漂移，輸出 bytes 都收斂成同一份。
//!   2. cache_control 自動放置：client 沒放標記才補；已有任何標記 = 客戶意圖，神聖不可侵犯。
//!   3. 老規矩：確定性、沒事做回 `Cow::Borrowed`（原始 bytes 本人）、壞輸入原樣放行。

use std::borrow::Cow;

use headroom_lite_rs::cache_stabilization::stabilize_request;
use serde_json::{json, Value};

fn tool_read() -> Value {
    json!({
        "name": "read_file",
        "description": "讀檔",
        "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
    })
}

fn tool_write() -> Value {
    // key 順序故意亂放（type 在 properties 前、path 在 content 前）—— 正規化後要遞迴排序
    json!({
        "name": "write_file",
        "description": "寫檔",
        "input_schema": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}},
    })
}

fn body(tools: Vec<Value>, system: Option<Value>, messages: Option<Value>) -> Vec<u8> {
    let mut b = json!({"model": "claude-opus-4-8", "tools": tools});
    let obj = b.as_object_mut().unwrap();
    if let Some(s) = system {
        obj.insert("system".into(), s);
    }
    obj.insert(
        "messages".into(),
        messages.unwrap_or_else(|| json!([{"role": "user", "content": "hi"}])),
    );
    serde_json::to_vec(&b).unwrap()
}

fn system_blocks() -> Value {
    json!([{"type": "text", "text": "你是嚴謹的助理。"}])
}

fn three_turns() -> Value {
    json!([
        {"role": "user", "content": [{"type": "text", "text": "第一問"}]},
        {"role": "assistant", "content": [{"type": "text", "text": "第一答"}]},
        {"role": "user", "content": [{"type": "text", "text": "第二問（live zone）"}]},
    ])
}

// ------------------------------------------------------- tools 正規化

#[test]
fn flickering_tool_order_converges_to_same_bytes() {
    // 殺手測試：同一組 tools、兩種順序 → 穩定化後 bytes 一模一樣。
    let turn_a = body(vec![tool_read(), tool_write()], None, None);
    let turn_b = body(vec![tool_write(), tool_read()], None, None);
    assert_ne!(turn_a, turn_b); // 入口確實不同
    assert_eq!(
        stabilize_request(&turn_a).into_owned(),
        stabilize_request(&turn_b).into_owned()
    ); // 出口收斂
}

#[test]
fn schema_keys_sorted_recursively() {
    let out: Value = serde_json::from_slice(&stabilize_request(&body(vec![tool_write()], None, None))).unwrap();
    let schema = out["tools"][0]["input_schema"].as_object().unwrap();
    let keys: Vec<&String> = schema.keys().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    let props: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    let mut props_sorted = props.clone();
    props_sorted.sort();
    assert_eq!(props, props_sorted);
}

// --------------------------------------------- cache_control 自動放置

#[test]
fn auto_breakpoints_when_client_has_none() {
    let raw = body(vec![tool_read()], Some(system_blocks()), Some(three_turns()));
    let out: Value = serde_json::from_slice(&stabilize_request(&raw)).unwrap();

    // system 最後一個 block 拿到標記（涵蓋 tools + system 前綴）
    let system = out["system"].as_array().unwrap();
    assert_eq!(system.last().unwrap()["cache_control"], json!({"type": "ephemeral"}));
    // live zone 之前的最後一則訊息（assistant）最後一個 block 拿到標記
    let messages = out["messages"].as_array().unwrap();
    let frozen_blocks = messages[messages.len() - 2]["content"].as_array().unwrap();
    assert_eq!(frozen_blocks.last().unwrap()["cache_control"], json!({"type": "ephemeral"}));
    // live zone 本身不放（它還會變，放了也快取不到東西）
    let live_blocks = messages.last().unwrap()["content"].as_array().unwrap();
    assert!(live_blocks.last().unwrap().get("cache_control").is_none());
}

#[test]
fn existing_markers_are_sacred() {
    // client 已放任何標記 → 我們一個都不加、也不動既有的。
    let mut messages = three_turns();
    messages[0]["content"][0]
        .as_object_mut()
        .unwrap()
        .insert("cache_control".into(), json!({"type": "ephemeral"}));
    let raw = body(vec![tool_read()], Some(system_blocks()), Some(messages));
    let out: Value = serde_json::from_slice(&stabilize_request(&raw)).unwrap();

    let mut marks = Vec::new();
    for (i, m) in out["messages"].as_array().unwrap().iter().enumerate() {
        for (j, b) in m["content"].as_array().unwrap().iter().enumerate() {
            if b.get("cache_control").is_some() {
                marks.push((i, j));
            }
        }
    }
    assert_eq!(marks, vec![(0, 0)]); // 只剩 client 自己放的那一個
    assert!(out["system"]
        .as_array()
        .unwrap()
        .iter()
        .all(|b| b.get("cache_control").is_none()));
}

// ------------------------------------------------------------- 老規矩

#[test]
fn already_canonical_is_borrowed_original_bytes() {
    // 沒事可做（tools 已排序、標記已存在）→ Cow::Borrowed = 原始 bytes 本人。
    let canonical_tool = json!({
        "name": "read_file",
        "description": "讀檔",
        // schema key 已是遞迴排序狀態（properties < type）
        "input_schema": {"properties": {"path": {"type": "string"}}, "type": "object"},
    });
    let mut messages = three_turns();
    messages[0]["content"][0]
        .as_object_mut()
        .unwrap()
        .insert("cache_control".into(), json!({"type": "ephemeral"}));
    let raw = body(vec![canonical_tool], None, Some(messages));
    assert!(matches!(stabilize_request(&raw), Cow::Borrowed(b) if b == raw.as_slice()));
}

#[test]
fn deterministic() {
    let raw = body(vec![tool_write(), tool_read()], Some(system_blocks()), Some(three_turns()));
    assert_eq!(
        stabilize_request(&raw).into_owned(),
        stabilize_request(&raw).into_owned()
    );
}

#[test]
fn bad_input_passes_through_borrowed() {
    assert!(matches!(stabilize_request(b"not json"), Cow::Borrowed(b"not json")));
}
