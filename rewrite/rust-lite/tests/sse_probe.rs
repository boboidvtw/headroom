//! M10 安全網：SSE 串流裡的 ccr_retrieve 被動觀察（observe-only）。
//!
//! 忠於工業版解答本（crates/headroom-proxy/src/sse/anthropic.rs）的選擇：
//! byte-passthrough 神聖不可侵，狀態機只「觀察」、不攔不改。這裡測的是
//! 「能不能在串流事件流裡，正確認出模型對 ccr_retrieve 的呼叫並取出 key」。
//!
//! tool_use 在 Anthropic 串流裡的長相（§5.1）：
//!   content_block_start (type=tool_use, name, index)
//!     content_block_delta (input_json_delta, partial_json) ×N  ← input 是逐段 JSON 字串
//!   content_block_stop (index)   ← 到這裡 partial_json 才湊齊、解析一次

use headroom_lite_rs::sse::SseCcrProbe;

/// 造一段含「一次 ccr_retrieve 呼叫」的 Anthropic SSE 串流文字。
/// input 的 partial_json 故意拆兩段送 —— 模擬真實逐段串流。
fn stream_with_tool(tool_name: &str, key: &str) -> Vec<u8> {
    let (head, tail) = key.split_at(key.len() / 2);
    [
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n",
        "event: content_block_start\n",
        &format!(
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"{tool_name}\",\"input\":{{}}}}}}\n\n"
        ),
        "event: content_block_delta\n",
        &format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"key\\\":\\\"{head}\"}}}}\n\n"
        ),
        "event: content_block_delta\n",
        &format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{tail}\\\"}}\"}}}}\n\n"
        ),
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    ]
    .concat()
    .into_bytes()
}

#[test]
fn detects_ccr_retrieve_key_fed_whole() {
    let mut probe = SseCcrProbe::new();
    let keys = probe.feed(&stream_with_tool("ccr_retrieve", "abc123def456"));
    assert_eq!(keys, vec!["abc123def456".to_string()]);
}

#[test]
fn detects_ccr_retrieve_across_tiny_chunks() {
    // 事件 / partial_json / 甚至 UTF-8 邊界都被切碎，仍要正確湊回 key
    let full = stream_with_tool("ccr_retrieve", "deadbeefcafe1234");
    let mut probe = SseCcrProbe::new();
    let mut keys = Vec::new();
    for chunk in full.chunks(5) {
        keys.extend(probe.feed(chunk));
    }
    assert_eq!(keys, vec!["deadbeefcafe1234".to_string()]);
}

#[test]
fn ignores_foreign_tool_use() {
    // 非 ccr_retrieve 的工具呼叫（client 自己的工具）不該被認成 ccr
    let mut probe = SseCcrProbe::new();
    let keys = probe.feed(&stream_with_tool("get_weather", "abc123def456"));
    assert!(keys.is_empty(), "foreign tool 不該被偵測，實得 {keys:?}");
}

#[test]
fn ignores_text_only_stream() {
    let text_stream = [
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]
    .concat()
    .into_bytes();
    let mut probe = SseCcrProbe::new();
    assert!(probe.feed(&text_stream).is_empty());
}
