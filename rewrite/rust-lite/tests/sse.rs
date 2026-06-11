//! M5 安全網：Rust port 的 byte-level SSE splitter。

use headroom_lite_rs::sse::SseByteSplitter;

fn event(text: &str) -> Vec<u8> {
    format!("event: content_block_delta\ndata: {{\"text\":\"{text}\"}}\n\n").into_bytes()
}

#[test]
fn emoji_split_across_chunks_preserved() {
    let raw = event("前🔥後");
    let fire = "🔥".as_bytes();
    let cut = raw.windows(fire.len()).position(|w| w == fire).unwrap() + 2;

    let mut splitter = SseByteSplitter::new();
    assert!(splitter.feed(&raw[..cut]).is_empty()); // 事件未完整，不吐
    let events = splitter.feed(&raw[cut..]);
    assert_eq!(events.len(), 1);
    assert!(String::from_utf8(events[0].clone()).unwrap().contains("前🔥後"));
}

#[test]
fn byte_by_byte_worst_case() {
    let raw = event("🔥 你好");
    let mut splitter = SseByteSplitter::new();
    let mut collected = Vec::new();
    for b in &raw {
        collected.extend(splitter.feed(std::slice::from_ref(b)));
    }
    assert_eq!(collected, vec![raw[..raw.len() - 2].to_vec()]);
}

#[test]
fn multiple_events_in_one_chunk() {
    let e1 = b"event: ping\ndata: {}\n\n".to_vec();
    let e2 = b"event: message_stop\ndata: {}\n\n".to_vec();
    let mut splitter = SseByteSplitter::new();
    let chunk: Vec<u8> = [e1.clone(), e2.clone()].concat();
    assert_eq!(
        splitter.feed(&chunk),
        vec![e1[..e1.len() - 2].to_vec(), e2[..e2.len() - 2].to_vec()]
    );
}

#[test]
fn crlf_boundaries_supported() {
    let raw = b"data: {}\r\n\r\n".to_vec();
    let mut splitter = SseByteSplitter::new();
    assert_eq!(splitter.feed(&raw), vec![b"data: {}".to_vec()]);
}
