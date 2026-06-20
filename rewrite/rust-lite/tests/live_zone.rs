//! M5 安全網：Rust port 的 live-zone 引擎。
//!
//! 與 Python 版相同的四鐵律，外加兩條 Rust 特有的：
//!   5. fallback 用型別作證 —— Cow::Borrowed 代表「原 bytes 本人」。
//!   6. arbitrary_precision —— 舊 turn 裡的 1.50 不准變 1.5。

use std::borrow::Cow;

use headroom_lite_rs::live_zone::compress_request;

fn huge_log() -> String {
    let mut lines = Vec::new();
    for _ in 0..5 {
        for i in 0..60 {
            lines.push(format!("2026-06-11T10:00:{i:02} INFO worker heartbeat ok"));
        }
    }
    lines.join("\n")
}

/// 手工組 raw JSON（不經 serde 序列化）—— 才能塞「非規範」數字
/// 字面值 1.50，驗證 parse → re-serialize 不會動它。
fn conversation(latest_tool_output: &str) -> Vec<u8> {
    let log = serde_json::to_string(&huge_log()).unwrap();
    let latest = serde_json::to_string(latest_tool_output).unwrap();
    format!(
        r#"{{"model":"claude-opus-4-8","temperature":1.50,"seed":12345678901234567,"messages":[{{"role":"user","content":"幫我看 log"}},{{"role":"assistant","content":[{{"type":"tool_use","id":"tu_1","name":"read_log","input":{{}}}}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"tu_1","content":{log}}}]}},{{"role":"assistant","content":"看完了"}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"tu_2","content":{latest}}},{{"type":"text","text":"再幫我總結"}}]}}]}}"#
    )
    .into_bytes()
}

#[test]
fn live_zone_tool_result_gets_compressed() {
    let raw = conversation(&huge_log());
    let out = compress_request(&raw);
    assert!(out.len() < raw.len());
}

#[test]
fn old_turns_are_never_touched() {
    let raw = conversation(&huge_log());
    let out = compress_request(&raw);
    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();

    // 舊 turn 的大 tool_result 一字不差
    assert_eq!(
        parsed["messages"][2]["content"][0]["content"].as_str().unwrap(),
        huge_log()
    );
    // live zone 裡使用者親手打的字不准動
    assert_eq!(
        parsed["messages"][4]["content"][1]["text"].as_str().unwrap(),
        "再幫我總結"
    );
}

#[test]
fn numeric_literals_preserved_via_arbitrary_precision() {
    // 殺手測試（PR-A4）：1.50 是「非規範」字面值，預設 f64 路徑
    // 會把它變成 1.5 —— bytes 變了，cache 炸。
    let raw = conversation(&huge_log());
    let out = compress_request(&raw);
    let out_str = std::str::from_utf8(&out).unwrap();
    assert!(out_str.contains(r#""temperature":1.50"#), "1.50 被改寫了");
    assert!(out_str.contains("12345678901234567"), "大整數掉精度");
}

#[test]
fn deterministic_same_input_same_output() {
    let raw = conversation(&huge_log());
    assert_eq!(compress_request(&raw).to_vec(), compress_request(&raw).to_vec());
}

#[test]
fn fallback_is_borrowed_original_bytes() {
    // Rust 獨有的升級：型別系統作證「沒壓就是原 bytes 本人」。
    let raw = conversation("short output");
    let out = compress_request(&raw);
    assert!(matches!(out, Cow::Borrowed(_)));
    assert_eq!(out.as_ref(), raw.as_slice());
}

#[test]
fn non_json_body_passes_through() {
    let raw = b"this is not json at all";
    let out = compress_request(raw);
    assert!(matches!(out, Cow::Borrowed(_)));
    assert_eq!(out.as_ref(), raw.as_slice());
}

#[test]
fn marker_format_matches_python_version() {
    // 跨語言 parity 的前提：標記格式逐字相同（Phase I 之魂）。
    // huge_log 是純 INFO log → M12 起走 log 策略（"dropped"），逐字節 parity 由
    // scripts/parity.sh 把關；這裡只鎖標記格式骨架。
    let raw = conversation(&huge_log());
    let out = compress_request(&raw);
    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let squeezed = parsed["messages"][4]["content"][0]["content"].as_str().unwrap();
    assert!(
        squeezed.contains("[... headroom-lite dropped ") && squeezed.contains(" log lines | sha256:"),
        "標記格式與 Python 版不一致"
    );
}
