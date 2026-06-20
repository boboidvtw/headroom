//! M8 安全網：lazy registration —— stabilize → compress →（有壓到才 register）。
//!
//! 對齊 Python 側 headroom_lite.pipeline.process_request。
//! Cow 接力契約：三段都沒動 → Borrowed（原始 bytes 本人）；
//! 有壓到才註冊 ccr_retrieve（接 tools 尾端，不重排 —— cache 前綴保留最大化）。

use std::borrow::Cow;

use headroom_lite_rs::ccr::CcrStore;
use headroom_lite_rs::pipeline::process_request;
use serde_json::{json, Value};

fn huge_log() -> String {
    let block: Vec<String> = (0..60)
        .map(|i| format!("2026-06-11T10:00:{i:02} INFO worker heartbeat ok"))
        .collect();
    block.join("\n").repeat(5)
}

#[test]
fn pipeline_applies_all_three_stages() {
    let raw = serde_json::to_vec(&json!({
        "model": "claude-opus-4-8",
        // tools 順序故意亂放（w 在 r 前）—— stabilize 要排序
        "tools": [
            {"name": "write_file", "description": "寫檔", "input_schema": {"type": "object"}},
            {"name": "read_file", "description": "讀檔", "input_schema": {"type": "object"}},
        ],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "第一問"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "第一答"}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": huge_log()},
            ]},
        ],
    }))
    .unwrap();

    let mut store = CcrStore::new();
    let out: Value = serde_json::from_slice(&process_request(&raw, Some(&mut store))).unwrap();

    let names: Vec<&str> = out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    // M3：client tools 按名字排序（read_file < write_file）
    // M8：有壓到 → ccr_retrieve 已註冊，且接在尾端（不排到最前面 —— cache 前綴保留最大化）
    assert_eq!(names, vec!["read_file", "write_file", "ccr_retrieve"]);
    // M3：零標記 → 自動補 cache_control（live zone 前的最後一則訊息）
    let messages = out["messages"].as_array().unwrap();
    let frozen_blocks = messages[messages.len() - 2]["content"].as_array().unwrap();
    assert_eq!(frozen_blocks.last().unwrap()["cache_control"], json!({"type": "ephemeral"}));
    // M1+M4：live zone 壓縮 + 原文可取回。不綁特定策略 —— log 內容走 log 策略
    // （"dropped"）、其他走 truncate（"squeezed"），共用標記前綴，斷言意圖＝壓到了。
    let squeezed = messages.last().unwrap()["content"][0]["content"].as_str().unwrap();
    assert!(squeezed.contains("[... headroom-lite "));
    assert_eq!(store.len(), 1);
}

#[test]
fn pipeline_lazy_skips_registration_when_nothing_compressed() {
    // M8 頭號契約：這輪沒壓到任何東西 → tools 不長出 ccr_retrieve（lazy 的核心）。
    let raw = serde_json::to_vec(&json!({
        "model": "claude-opus-4-8",
        "tools": [
            {"name": "read_file", "description": "讀檔", "input_schema": {"type": "object"}},
        ],
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "short output"},
            ]},
        ],
    }))
    .unwrap();

    let mut store = CcrStore::new();
    let out: Value = serde_json::from_slice(&process_request(&raw, Some(&mut store))).unwrap();
    let names: Vec<&str> = out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"ccr_retrieve")); // 沒壓 → 不註冊
    assert_eq!(store.len(), 0); // 沒收存任何原文
}

#[test]
fn pipeline_bad_input_is_borrowed_passthrough() {
    // 三段引擎的失敗模式契約必須穿透整條 pipeline：壞輸入 → 原始 bytes 本人。
    let mut store = CcrStore::new();
    assert!(matches!(
        process_request(b"not json", Some(&mut store)),
        Cow::Borrowed(b"not json")
    ));
}

#[test]
fn pipeline_is_deterministic() {
    let raw = serde_json::to_vec(&json!({
        "model": "m",
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "tu_1", "content": huge_log()},
        ]}],
    }))
    .unwrap();
    let a = process_request(&raw, None).into_owned();
    let b = process_request(&raw, None).into_owned();
    assert_eq!(a, b);
}
