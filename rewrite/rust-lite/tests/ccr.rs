//! M6 安全網：CCR 可逆取回 port 到 Rust（與 Python tests/test_ccr.py 對齊）。
//!
//!   1. 壓縮永不丟資料 —— 原文存進 content-addressed store，標記裡的 sha256 就是取回 key。
//!   2. ccr_retrieve 工具「每請求都註冊」—— 包括沒壓到東西的請求。
//!      時有時無 = tools 陣列閃爍 = cache 前綴炸掉（原版的 bug）。
//!   3. 工具定義 bytes 跨輪逐字節穩定。

use std::borrow::Cow;

use headroom_lite_rs::ccr::{handle_retrieve, register_ccr_tool, CcrStore};
use headroom_lite_rs::live_zone::compress_request_with_store;
use serde_json::{json, Value};

fn huge_log() -> String {
    let block: Vec<String> = (0..60)
        .map(|i| format!("2026-06-11T10:00:{i:02} INFO worker heartbeat ok"))
        .collect();
    block.join("\n").repeat(5)
}

fn body_with_big_tool_result() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "claude-opus-4-8",
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": huge_log()},
            ]},
        ],
    }))
    .unwrap()
}

// ----------------------------------------------------------- 可逆性

#[test]
fn squeezed_original_is_retrievable() {
    // 殺手測試：壓掉的內容，用標記裡的 key 一字不差取回。
    let mut store = CcrStore::new();
    let raw = body_with_big_tool_result(); // Cow 借用輸入 —— 輸入必須活得比輸出久
    let out = compress_request_with_store(&raw, Some(&mut store));

    let parsed: Value = serde_json::from_slice(&out).unwrap();
    let squeezed = parsed["messages"][0]["content"][0]["content"].as_str().unwrap();
    let key_start = squeezed.find("sha256:").unwrap() + "sha256:".len();
    let key = &squeezed[key_start..key_start + 16];
    assert_eq!(store.get(key), Some(huge_log().as_str()));
}

#[test]
fn store_is_content_addressed() {
    let mut store = CcrStore::new();
    let k1 = store.put("same text");
    let k2 = store.put("same text");
    assert_eq!(k1, k2); // 同文同 key
    assert_eq!(store.len(), 1); // 自動去重
    assert_eq!(store.get(&k1), Some("same text"));
    assert_eq!(store.get(&"0".repeat(16)), None); // 未知 key → None，不炸
}

#[test]
fn compress_without_store_still_works() {
    // store 是可選的 —— M1/M5 的行為完全不變（向後相容）。
    let raw = body_with_big_tool_result();
    let out = headroom_lite_rs::live_zone::compress_request(&raw);
    assert!(out.len() < raw.len());
}

#[test]
fn retrieve_unknown_key_is_honest() {
    let store = CcrStore::new();
    let reply = handle_retrieve(&store, "deadbeefdeadbeef");
    assert!(reply.contains("deadbeefdeadbeef")); // 誠實說找不到，帶上 key 方便排查
}

// ------------------------------------------------- 每請求都註冊

#[test]
fn tool_registered_even_when_nothing_compressed() {
    // 鐵律 4 核心：這輪沒壓到任何東西，工具照樣要在。
    let tiny = serde_json::to_vec(&json!({
        "model": "claude-opus-4-8",
        "tools": [{"name": "read_file", "description": "讀檔",
                   "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "hi"}],
    }))
    .unwrap();
    let out: Value = serde_json::from_slice(&register_ccr_tool(&tiny)).unwrap();
    assert!(out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "ccr_retrieve"));
}

#[test]
fn tool_definition_bytes_stable_across_turns() {
    // 工具定義必須跨輪逐字節相同 —— 差一個 byte，tools 前綴就炸。
    let turn = |text: &str| {
        serde_json::to_vec(&json!({"model": "m", "tools": [],
            "messages": [{"role": "user", "content": text}]}))
        .unwrap()
    };
    let extract = |raw: &[u8]| -> Vec<u8> {
        let parsed: Value = serde_json::from_slice(raw).unwrap();
        let def: Vec<&Value> = parsed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["name"] == "ccr_retrieve")
            .collect();
        serde_json::to_vec(&def).unwrap()
    };
    assert_eq!(
        extract(&register_ccr_tool(&turn("a"))),
        extract(&register_ccr_tool(&turn("b")))
    );
}

#[test]
fn no_tools_array_gets_one_created() {
    let no_tools = serde_json::to_vec(&json!({"model": "m",
        "messages": [{"role": "user", "content": "hi"}]}))
    .unwrap();
    let out: Value = serde_json::from_slice(&register_ccr_tool(&no_tools)).unwrap();
    let names: Vec<&str> = out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["ccr_retrieve"]);
}

#[test]
fn already_registered_is_borrowed_original_bytes() {
    // 已經註冊過（例如上一層 middleware 做了）→ Cow::Borrowed = 原始 bytes 本人。
    let raw = register_ccr_tool(
        &serde_json::to_vec(&json!({"model": "m", "tools": [],
            "messages": [{"role": "user", "content": "hi"}]}))
        .unwrap(),
    )
    .into_owned();
    assert!(matches!(register_ccr_tool(&raw), Cow::Borrowed(b) if b == raw.as_slice()));
}

#[test]
fn bad_input_passes_through_borrowed() {
    assert!(matches!(register_ccr_tool(b"not json"), Cow::Borrowed(b"not json")));
}
