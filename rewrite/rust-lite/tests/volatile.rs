//! M23 — volatile 唯讀掃描（Rust 側）。
//!
//! 與 Python `tests/test_volatile.py` 一對一鏡像：同樣的輸入必須得到
//! 同樣的 findings。兩份實作互為守門（READING-03 的教訓：port 的價值
//! 不在多一份程式碼，在於一邊寫錯時另一邊會吵）。

use headroom_lite_rs::volatile::{
    scan_request, VolatileKind, MAX_FINDINGS, SAMPLE_MAX_CHARS,
};

// ─── 1. timestamp ──────────────────────────────────────────────────────

#[test]
fn detects_iso8601_timestamp_in_system_prompt() {
    let raw = br#"{"system":"Today is 2026-05-04T14:30:00Z. Be concise."}"#;
    let findings = scan_request(raw);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::Timestamp);
    assert_eq!(findings[0].location, "system");
    assert_eq!(findings[0].sample, "2026-05-04T14:30:00");
}

#[test]
fn iso8601_with_space_separator_recognized() {
    let raw = br#"{"system":"started at 2026-05-04 14:30:00"}"#;
    let findings = scan_request(raw);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::Timestamp);
}

#[test]
fn non_ascii_digits_are_not_digits() {
    // parity 地雷的另一半：Rust 逐 byte 掃，非 ASCII 天生不匹配。
    let raw = r#"{"system":"٢٠٢٦-٠٥-٠٤T١٤:٣٠:٠٠"}"#.as_bytes();
    assert!(scan_request(raw).is_empty());
}

// ─── 2. uuid v4 ────────────────────────────────────────────────────────

#[test]
fn detects_uuid_v4_in_user_message() {
    let raw = br#"{"messages":[{"role":"user","content":"trace=550e8400-e29b-41d4-a716-446655440000"},{"role":"user","content":"and now?"}]}"#;
    let findings = scan_request(raw);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::Uuid);
    assert_eq!(findings[0].location, "messages[0].content");
    assert_eq!(findings[0].sample, "550e8400-e29b-41d4-a716-446655440000");
}

#[test]
fn random_hex_without_v4_nibble_is_not_a_uuid() {
    let raw = br#"{"messages":[{"role":"user","content":"id=550e8400-e29b-01d4-a716-446655440000"},{"role":"user","content":"and now?"}]}"#;
    let findings = scan_request(raw);
    assert!(findings.iter().all(|f| f.kind != VolatileKind::Uuid));
}

#[test]
fn uuid_with_bad_variant_nibble_is_not_v4() {
    let raw = br#"{"messages":[{"role":"user","content":"550e8400-e29b-41d4-c716-446655440000"},{"role":"user","content":"and now?"}]}"#;
    let findings = scan_request(raw);
    assert!(findings.iter().all(|f| f.kind != VolatileKind::Uuid));
}

// ─── 3. ID 名稱欄位 ────────────────────────────────────────────────────

#[test]
fn detects_request_id_field_in_nested_schema() {
    let raw = br#"{"tools":[{"name":"lookup","description":"Look up a user.","input_schema":{"type":"object","properties":{"user_id":{"type":"string"},"request_id":"req-2026-abc-12345"}}}]}"#;
    let findings = scan_request(raw);
    let id_fields: Vec<_> = findings
        .iter()
        .filter(|f| f.kind == VolatileKind::IdField)
        .collect();
    assert_eq!(id_fields.len(), 1);
    assert_eq!(
        id_fields[0].location,
        "tools[0].input_schema.properties.request_id"
    );
    assert_eq!(id_fields[0].sample, "req-2026-abc-12345");
}

#[test]
fn id_field_with_empty_value_does_not_fire() {
    let raw = br#"{"tools":[{"input_schema":{"properties":{"request_id":""}}}]}"#;
    let findings = scan_request(raw);
    assert!(findings.iter().all(|f| f.kind != VolatileKind::IdField));
}

#[test]
fn id_field_name_match_is_ascii_case_insensitive_substring() {
    let raw = br#"{"tools":[{"input_schema":{"X_Request_ID":7}}]}"#;
    let findings = scan_request(raw);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::IdField);
    assert_eq!(findings[0].location, "tools[0].input_schema.X_Request_ID");
}

#[test]
fn numeric_id_value_sample_keeps_original_literal() {
    // arbitrary_precision 讓 `1.10` 保持字面值；Python 端靠 parse_float=str 對齊。
    let raw = br#"{"tools":[{"input_schema":{"trace_id":1.10}}]}"#;
    let findings = scan_request(raw);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].sample, "1.10");
}

#[test]
fn number_literals_never_look_like_timestamp_or_uuid() {
    let raw = br#"{"system":[{"type":"text","value":1e10}]}"#;
    assert!(scan_request(raw).is_empty());
}

// ─── 4. 不誤報 / 上限 / 非變性 ────────────────────────────────────────

#[test]
fn stable_content_yields_zero_findings() {
    let raw = br#"{"system":"You are a helpful assistant. Be concise.","messages":[{"role":"user","content":"Summarize the document below."},{"role":"assistant","content":"Sure - please paste it."}],"tools":[{"name":"search","description":"Search the corpus.","input_schema":{"type":"object","properties":{"query":{"type":"string"}}}}]}"#;
    let findings = scan_request(raw);
    assert!(findings.is_empty(), "誤報：{findings:?}");
}

#[test]
fn caps_findings() {
    let mut messages = String::from("[");
    for i in 0..30 {
        if i > 0 {
            messages.push(',');
        }
        messages.push_str(&format!(
            r#"{{"role":"user","content":"turn {i}: 550e8400-e29b-41d4-a716-446655440000"}}"#
        ));
    }
    messages.push(']');
    let raw = format!(r#"{{"messages":{messages}}}"#);
    assert_eq!(scan_request(raw.as_bytes()).len(), MAX_FINDINGS);
}

#[test]
fn sample_is_truncated_with_ellipsis() {
    let long_value = "x".repeat(SAMPLE_MAX_CHARS + 50);
    let raw = format!(r#"{{"tools":[{{"input_schema":{{"session_id":"{long_value}"}}}}]}}"#);
    let findings = scan_request(raw.as_bytes());
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].sample, format!("{}…", "x".repeat(SAMPLE_MAX_CHARS)));
}

#[test]
fn sample_truncation_is_char_based_not_byte_based() {
    // 刻意偏離解答本（它切 80 bytes）：切字元才能與 Python 吐出同一串。
    let long_value = "汉".repeat(SAMPLE_MAX_CHARS + 10); // 每字 3 bytes
    let raw = format!(r#"{{"tools":[{{"input_schema":{{"session_id":"{long_value}"}}}}]}}"#);
    let findings = scan_request(raw.as_bytes());
    assert_eq!(findings.len(), 1);
    assert_eq!(
        findings[0].sample,
        format!("{}…", "汉".repeat(SAMPLE_MAX_CHARS)),
        "截斷必須以字元計；以 byte 計會在這裡切出 27 個字",
    );
}

#[test]
fn malformed_input_returns_no_findings() {
    assert!(scan_request(b"not json at all").is_empty());
    assert!(scan_request(b"[1,2,3]").is_empty());
    assert!(scan_request(&[0xff, 0xfe]).is_empty());
}

// ─── 5. live zone 不掃（照抄解答本會踩的坑）──────────────────────────

#[test]
fn live_zone_volatile_content_is_not_reported() {
    // 最後一則永遠在快取前綴之外（M3 標記 2 放 messages[-2]）。
    let raw = br#"{"system":"You are a build assistant.","messages":[{"role":"user","content":"hello"},{"role":"user","content":[{"type":"tool_result","content":"2026-06-11T10:00:00 INFO ok\n2026-06-11T10:00:01 INFO ok"}]}]}"#;
    assert!(scan_request(raw).is_empty());
}

#[test]
fn frozen_history_is_still_reported() {
    let raw = br#"{"messages":[{"role":"user","content":"started 2026-06-11T10:00:00Z"},{"role":"assistant","content":"ok"}]}"#;
    let findings = scan_request(raw);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::Timestamp);
    assert_eq!(findings[0].location, "messages[0].content");
}

#[test]
fn live_zone_noise_does_not_crowd_out_real_findings() {
    // 上限是全域的：live zone 若能貢獻 findings，光它一則就灌滿 10 筆，
    // 把 tools 裡真正該報的東西安靜擠掉。
    let mut noisy = String::new();
    for i in 0..40 {
        if i > 0 {
            noisy.push_str("\\n");
        }
        noisy.push_str(&format!("2026-06-11T10:00:{i:02} INFO tick"));
    }
    let raw = format!(
        r#"{{"messages":[{{"role":"user","content":"go"}},{{"role":"user","content":[{{"type":"tool_result","content":"{noisy}"}}]}}],"tools":[{{"name":"lookup","input_schema":{{"properties":{{"correlation_id":"ci-0417"}}}}}}]}}"#
    );
    let findings = scan_request(raw.as_bytes());
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].kind, VolatileKind::IdField);
    assert_eq!(
        findings[0].location,
        "tools[0].input_schema.properties.correlation_id"
    );
}

#[test]
fn single_message_body_scans_nothing() {
    let raw = br#"{"messages":[{"role":"user","content":"at 2026-06-11T10:00:00Z"}]}"#;
    assert!(scan_request(raw).is_empty());
}

// ─── 6. 路徑（location）正確性 ─────────────────────────────────────────

#[test]
fn locations_for_block_lists() {
    let raw = br#"{"system":[{"type":"text","text":"now=2026-05-04T14:30:00Z"}],"messages":[{"role":"user","content":[{"type":"text","text":"id=550e8400-e29b-41d4-a716-446655440000"}]},{"role":"user","content":[{"type":"text","text":"and now?"}]}],"tools":[{"name":"t","description":"since 2026-01-01T00:00:00Z"}]}"#;
    let findings = scan_request(raw);
    let got: Vec<(VolatileKind, &str)> = findings
        .iter()
        .map(|f| (f.kind, f.location.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (VolatileKind::Timestamp, "system[0].text"),
            (VolatileKind::Uuid, "messages[0].content[0].text"),
            (VolatileKind::Timestamp, "tools[0].description"),
        ]
    );
}
