//! M23 — volatile 唯讀掃描（Rust 側）。
//!
//! 與 Python `tests/test_volatile.py` 一對一鏡像：同樣的輸入必須得到
//! 同樣的 findings。兩份實作互為守門（READING-03 的教訓：port 的價值
//! 不在多一份程式碼，在於一邊寫錯時另一邊會吵）。
//!
//! 2026-08-10 code review 後補：邊界輸入的差分覆蓋在
//! `tests/fixtures/volatile/` 的 adversarial gate（parity.sh 相 3），
//! 這裡放的是單語言就說得清楚的行為。

use headroom_lite_rs::volatile::{
    detect_volatile_content, scan_request, VolatileKind, MAX_FINDINGS, MAX_LOCATION_CHARS,
    MAX_LOCATION_SEGMENT_CHARS, MAX_SCAN_BYTES,
};

// ─── 1. timestamp ──────────────────────────────────────────────────────

#[test]
fn detects_iso8601_timestamp_in_system_prompt() {
    let raw = br#"{"system":"Today is 2026-05-04T14:30:00Z. Be concise."}"#;
    let findings = scan_request(raw).findings;
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::Timestamp);
    assert_eq!(findings[0].location, "system");
    assert_eq!(findings[0].sample, "2026-05-04T14:30:00");
}

#[test]
fn iso8601_with_space_separator_recognized() {
    let raw = br#"{"system":"started at 2026-05-04 14:30:00"}"#;
    let findings = scan_request(raw).findings;
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::Timestamp);
}

#[test]
fn non_ascii_digits_are_not_digits() {
    // parity 地雷的另一半：Rust 逐 byte 掃，非 ASCII 天生不匹配。
    let raw = r#"{"system":"٢٠٢٦-٠٥-٠٤T١٤:٣٠:٠٠"}"#.as_bytes();
    assert!(scan_request(raw).findings.is_empty());
}

// ─── 2. uuid v4 ────────────────────────────────────────────────────────

#[test]
fn detects_uuid_v4_in_user_message() {
    let raw = br#"{"messages":[{"role":"user","content":"trace=550e8400-e29b-41d4-a716-446655440000"},{"role":"user","content":"and now?"}]}"#;
    let findings = scan_request(raw).findings;
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::Uuid);
    assert_eq!(findings[0].location, "messages[0].content");
    // v4 形狀的 API key 很常見；定位靠 location 就夠。
    assert_eq!(findings[0].sample, "550e8400…");
}

#[test]
fn random_hex_without_v4_nibble_is_not_a_uuid() {
    let raw = br#"{"messages":[{"role":"user","content":"id=550e8400-e29b-01d4-a716-446655440000"},{"role":"user","content":"and now?"}]}"#;
    let findings = scan_request(raw).findings;
    assert!(findings.iter().all(|f| f.kind != VolatileKind::Uuid));
}

#[test]
fn uuid_with_bad_variant_nibble_is_not_v4() {
    let raw = br#"{"messages":[{"role":"user","content":"550e8400-e29b-41d4-c716-446655440000"},{"role":"user","content":"and now?"}]}"#;
    let findings = scan_request(raw).findings;
    assert!(findings.iter().all(|f| f.kind != VolatileKind::Uuid));
}

// ─── 3. ID 名稱欄位與 sample 政策 ──────────────────────────────────────

#[test]
fn detects_request_id_field_in_nested_schema() {
    let raw = br#"{"tools":[{"name":"lookup","description":"Look up a user.","input_schema":{"type":"object","properties":{"user_id":{"type":"string"},"request_id":"req-2026-abc-12345"}}}]}"#;
    let findings = scan_request(raw).findings;
    let id_fields: Vec<_> = findings
        .iter()
        .filter(|f| f.kind == VolatileKind::IdField)
        .collect();
    assert_eq!(id_fields.len(), 1);
    assert_eq!(
        id_fields[0].location,
        "tools[0].input_schema.properties.request_id"
    );
    assert_eq!(id_fields[0].sample, "string[18]");
}

#[test]
fn id_field_with_empty_value_does_not_fire() {
    let raw = br#"{"tools":[{"input_schema":{"properties":{"request_id":""}}}]}"#;
    let findings = scan_request(raw).findings;
    assert!(findings.iter().all(|f| f.kind != VolatileKind::IdField));
}

#[test]
fn id_field_name_match_is_ascii_case_insensitive_substring() {
    let raw = br#"{"tools":[{"input_schema":{"X_Request_ID":7}}]}"#;
    let findings = scan_request(raw).findings;
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::IdField);
    assert_eq!(findings[0].location, "tools[0].input_schema.X_Request_ID");
}

#[test]
fn id_field_sample_never_contains_the_value() {
    // needle 是子字串比對，`session_identity_token` 命中 `session_id` ——
    // 而這種欄位在很多系統裡本身就是憑證。命中集合是開放的、列舉不完，
    // 唯一安全的作法是永遠不回吐值。
    let secret = "sk-ant-api03-REDACTEDSECRET-abcdefghij";
    let raw =
        format!(r#"{{"tools":[{{"input_schema":{{"session_identity_token":"{secret}"}}}}]}}"#);
    let findings = scan_request(raw.as_bytes()).findings;
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::IdField);
    assert_eq!(
        findings[0].sample,
        format!("string[{}]", secret.chars().count())
    );
    assert!(!findings[0].sample.contains("REDACTEDSECRET"));
}

#[test]
fn id_field_sample_describes_type_and_size() {
    for (value, expected) in [
        (r#""abc""#, "string[3]"),
        ("7", "number"),
        ("true", "bool"),
        ("[1,2,3]", "array[3]"),
        (r#"{"a":1,"b":2}"#, "object[2]"),
    ] {
        let raw = format!(r#"{{"tools":[{{"input_schema":{{"trace_id":{value}}}}}]}}"#);
        let findings = scan_request(raw.as_bytes()).findings;
        assert_eq!(findings[0].sample, expected, "value={value}");
    }
}

#[test]
fn string_length_in_sample_counts_characters_not_bytes() {
    // Rust 用 chars().count()，Python 用 len(str) —— 用 byte 會讓非 ASCII
    // 值的描述在兩邊分岔（每個漢字 3 bytes）。
    let raw = r#"{"tools":[{"input_schema":{"trace_id":"汉字漢"}}]}"#;
    let findings = scan_request(raw.as_bytes()).findings;
    assert_eq!(findings[0].sample, "string[3]");
}

#[test]
fn numeric_id_value_is_never_rendered_as_a_literal() {
    // 這條測試的前身斷言「arbitrary_precision 保留原始字面值」——
    // 那句話只在小數尾隨零成立，我只驗了 `1.10` 一個例子就推廣了。
    // `1E5` 會被正規化成 `1e+5`、`-0` 變 `0`；三種都測。
    for raw in [
        &br#"{"tools":[{"input_schema":{"trace_id":1.10}}]}"#[..],
        &br#"{"tools":[{"input_schema":{"trace_id":1E5}}]}"#[..],
        &br#"{"tools":[{"input_schema":{"trace_id":-0}}]}"#[..],
    ] {
        let findings = scan_request(raw).findings;
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].sample, "number");
    }
}

#[test]
fn number_literals_never_look_like_timestamp_or_uuid() {
    let raw = br#"{"system":[{"type":"text","value":1e10}]}"#;
    assert!(scan_request(raw).findings.is_empty());
}

// ─── 4. 不誤報 / 上限 / 非變性 ────────────────────────────────────────

#[test]
fn stable_content_yields_zero_findings() {
    let raw = br#"{"system":"You are a helpful assistant. Be concise.","messages":[{"role":"user","content":"Summarize the document below."},{"role":"assistant","content":"Sure - please paste it."}],"tools":[{"name":"search","description":"Search the corpus.","input_schema":{"type":"object","properties":{"query":{"type":"string"}}}}]}"#;
    let findings = scan_request(raw).findings;
    assert!(findings.is_empty(), "誤報：{findings:?}");
}

#[test]
fn caps_distinct_locations_and_signals_truncation() {
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
    let scan = scan_request(raw.as_bytes());
    assert_eq!(scan.findings.len(), MAX_FINDINGS);
    assert!(scan.truncated, "撞上限與『剛好 10 筆』不能長得一樣");
}

#[test]
fn repeated_hits_in_one_location_share_a_slot() {
    // 上限算的是相異位置不是命中次數，否則噪音會把 tools 裡真正該報的
    // 東西安靜擠掉 —— 噪音之外還會漏報。
    let mut noisy = String::new();
    for i in 0..40 {
        if i > 0 {
            noisy.push(' ');
        }
        noisy.push_str(&format!("2026-06-11T10:00:{i:02}"));
    }
    let raw = format!(
        r#"{{"messages":[{{"role":"user","content":"{noisy}"}},{{"role":"user","content":"live zone"}}],"tools":[{{"input_schema":{{"correlation_id":"ci-1"}}}}]}}"#
    );
    let scan = scan_request(raw.as_bytes());
    assert!(!scan.truncated);
    let got: Vec<(VolatileKind, &str, usize)> = scan
        .findings
        .iter()
        .map(|f| (f.kind, f.location.as_str(), f.count))
        .collect();
    assert_eq!(
        got,
        vec![
            (VolatileKind::Timestamp, "messages[0].content", 40),
            (
                VolatileKind::IdField,
                "tools[0].input_schema.correlation_id",
                1
            ),
        ]
    );
}

#[test]
fn oversized_body_is_skipped_with_its_own_signal() {
    // 這條路徑跑在轉發之前，掃多久就是延遲多久。
    // **訊號不可與撞上限共用**：初版兩者共用 truncated，於是 proxy 會對一份
    // 根本沒掃過的 body 印出「已達 10 個相異位置的上限」——事實錯誤的觀測線。
    let raw = format!(r#"{{"system":"{}"}}"#, "x".repeat(MAX_SCAN_BYTES + 1));
    let scan = scan_request(raw.as_bytes());
    assert!(scan.findings.is_empty());
    assert!(scan.skipped_too_large, "放棄掃描必須留下訊號");
    assert!(
        !scan.truncated,
        "沒掃過的 body 不該宣稱『撞到 findings 上限』"
    );
}

#[test]
fn rejected_inputs_set_no_signal_at_all() {
    // 拒收路徑不得誤設任何旗標，否則下游會把「看不懂」當成「還有更多」。
    let nan = br#"{"system":"2026-05-04T14:30:00Z","x":NaN}"#;
    for raw in [&b"not json"[..], &nan[..], &[0xff, 0xfe][..]] {
        let scan = scan_request(raw);
        assert!(scan.findings.is_empty());
        assert!(!scan.truncated && !scan.skipped_too_large, "{raw:?}");
    }
}

// ─── 4b. location 設界（review 二輪）──────────────────────────────────

#[test]
fn location_segment_is_capped() {
    // location 是**客戶 key 名**串起來的路徑，祖先 key 完全不受 needle 約束
    // —— 一份用 email 或 token 當 map key 的 JSON，整串都會進觀測線。
    let long_key = "A".repeat(200);
    let raw = format!(r#"{{"tools":[{{"input_schema":{{"{long_key}":{{"trace_id":"v"}}}}}}]}}"#);
    let findings = scan_request(raw.as_bytes()).findings;
    assert_eq!(
        findings[0].location,
        format!(
            "tools[0].input_schema.{}….trace_id",
            "A".repeat(MAX_LOCATION_SEGMENT_CHARS)
        )
    );
}

#[test]
fn location_total_length_is_capped() {
    // 沒有總長上限的話，200 KB 的 key 名就是 200 KB 的單行 stderr。
    let mut nested = String::from(r#"{"trace_id":"v"}"#);
    for i in 0..40 {
        nested = format!(r#"{{"segment_{i}_{}":{nested}}}"#, "x".repeat(30));
    }
    let raw = format!(r#"{{"tools":[{{"input_schema":{nested}}}]}}"#);
    let findings = scan_request(raw.as_bytes()).findings;
    let loc = &findings[0].location;
    assert!(
        loc.chars().count() <= MAX_LOCATION_CHARS + 1,
        "{}",
        loc.chars().count()
    );
    assert!(loc.ends_with(".trace_id"), "尾端命中欄位必須保留：{loc}");
    assert!(loc.starts_with("tools[0]"), "頭部定位資訊必須保留：{loc}");
}

#[test]
fn deep_value_via_public_api_is_bounded_by_the_depth_guard() {
    // `detect_volatile_content` 是公開 API、收已建好的 Value，繞過了
    // serde_json parser 的深度上限 —— 所以走訪內要再擋一次。
    //
    // 誠實界線：這道守門讓**走訪**有界，但沒辦法讓任意深的 `Value` 變安全
    // —— 夠深的 `Value` 光是被 drop 就會讓 serde_json 自己的遞迴 `Drop`
    // stack overflow 並 abort（SIGABRT，直接殺掉整個 test binary，不是測試
    // 失敗），那發生在本模組之外、也在守門之外。
    //
    // **門檻隨當下的 stack 大小而異**：libtest 的 worker thread 上約 2000 層
    // 就 abort，8 MiB 的 main thread 撐得到 5000。所以這裡取 300 —— 仍是
    // MAX_DEPTH(127) 的 2.4 倍、守門確實會觸發，而離 abort 門檻夠遠。
    // （初版寫 1000 並在註解裡說「5000 才會 abort」，那個 5000 是量測當下那個
    //   stack 的產物、不是門檻，會讓讀者以為安全邊際比實際大。）
    let mut deep = serde_json::json!({"trace_id": "2026-05-04T14:30:00Z"});
    for _ in 0..300 {
        deep = serde_json::json!({ "nest": deep });
    }
    let scan = detect_volatile_content(&serde_json::json!({ "system": deep }));
    assert!(
        scan.findings.is_empty(),
        "超過 MAX_DEPTH 的部分不該被走訪：{:?}",
        scan.findings
    );
}

#[test]
fn malformed_input_returns_no_findings() {
    assert!(scan_request(b"not json at all").findings.is_empty());
    assert!(scan_request(b"[1,2,3]").findings.is_empty());
    assert!(scan_request(&[0xff, 0xfe]).findings.is_empty());
}

#[test]
fn input_convergence_matches_python_side() {
    // 這三類 Rust 是 serde_json 免費擋掉的，Python 那側全部得手動對齊
    // （NaN / 非 UTF-8 BOM / 落單 surrogate）。
    let nan = br#"{"system":"2026-05-04T14:30:00Z","x":NaN}"#;
    let bom = b"\xef\xbb\xbf{\"system\":\"2026-05-04T14:30:00Z\"}";
    let lone = b"{\"system\":\"\\ud800 2026-05-04T14:30:00Z\"}";
    for raw in [&nan[..], &bom[..], &lone[..]] {
        assert!(scan_request(raw).findings.is_empty(), "應拒收：{raw:?}");
    }
    // 但成對 surrogate 是合法 JSON，不可以被一起擋掉 —— 守門要同時測
    // 「該擋的擋了」與「該過的還會過」。
    let paired = scan_request(b"{\"system\":\"\\ud83d\\ude00 2026-05-04T14:30:00Z\"}");
    assert_eq!(paired.findings.len(), 1);
    assert_eq!(paired.findings[0].kind, VolatileKind::Timestamp);
}

#[test]
fn deep_nesting_boundary() {
    // serde_json 的 parse 深度上限是 128 層容器；**兩側都釘** ——
    // 只釘一側的話另一側漂了不會有人知道。
    let nest = |d: usize| {
        format!(
            "{{\"system\":{}\"2026-05-04T14:30:00Z\"{}}}",
            "[".repeat(d),
            "]".repeat(d)
        )
    };
    assert_eq!(scan_request(nest(126).as_bytes()).findings.len(), 1);
    assert!(scan_request(nest(127).as_bytes()).findings.is_empty());
    assert!(scan_request(nest(5000).as_bytes()).findings.is_empty());
}

// ─── 5. live zone 不掃（照抄解答本會踩的坑）──────────────────────────

#[test]
fn live_zone_volatile_content_is_not_reported() {
    // 最後一則永遠在快取前綴之外（M3 標記 2 放 messages[-2]）。
    let raw = br#"{"system":"You are a build assistant.","messages":[{"role":"user","content":"hello"},{"role":"user","content":[{"type":"tool_result","content":"2026-06-11T10:00:00 INFO ok"}]}]}"#;
    assert!(scan_request(raw).findings.is_empty());
}

#[test]
fn frozen_history_is_still_reported() {
    let raw = br#"{"messages":[{"role":"user","content":"started 2026-06-11T10:00:00Z"},{"role":"assistant","content":"ok"}]}"#;
    let findings = scan_request(raw).findings;
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, VolatileKind::Timestamp);
    assert_eq!(findings[0].location, "messages[0].content");
}

#[test]
fn live_zone_noise_does_not_crowd_out_real_findings() {
    let mut noisy = String::new();
    for i in 0..40 {
        if i > 0 {
            noisy.push(' ');
        }
        noisy.push_str(&format!("2026-06-11T10:00:{i:02} INFO tick"));
    }
    let raw = format!(
        r#"{{"messages":[{{"role":"user","content":"go"}},{{"role":"user","content":[{{"type":"tool_result","content":"{noisy}"}}]}}],"tools":[{{"name":"lookup","input_schema":{{"properties":{{"correlation_id":"ci-0417"}}}}}}]}}"#
    );
    let findings = scan_request(raw.as_bytes()).findings;
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
    assert!(scan_request(raw).findings.is_empty());
}

// ─── 6. 路徑（location）正確性 ─────────────────────────────────────────

#[test]
fn locations_for_block_lists() {
    let raw = br#"{"system":[{"type":"text","text":"now=2026-05-04T14:30:00Z"}],"messages":[{"role":"user","content":[{"type":"text","text":"id=550e8400-e29b-41d4-a716-446655440000"}]},{"role":"user","content":[{"type":"text","text":"and now?"}]}],"tools":[{"name":"t","description":"since 2026-01-01T00:00:00Z"}]}"#;
    let findings = scan_request(raw).findings;
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
