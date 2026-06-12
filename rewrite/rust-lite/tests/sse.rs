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

// ---- M7：boundary-preserving frames（proxy 回程重切用）----
//
// `feed` 吐的事件「不含」邊界 bytes —— 拿來重組回程會丟失
// 「這事件原本是 \n\n 還是 \r\n\r\n 結尾」的資訊，byte-faithful 必炸。
// `feed_frames` 吐的 frame「含」原始邊界 bytes，數學性質：
//   concat(所有 frames) + take_remaining() == 所有餵進去的 bytes

#[test]
fn frames_include_original_boundary_bytes() {
    let mut splitter = SseByteSplitter::new();
    let frames = splitter.feed_frames(b"data: a\n\ndata: b\r\n\r\n");
    assert_eq!(
        frames,
        vec![b"data: a\n\n".to_vec(), b"data: b\r\n\r\n".to_vec()]
    );
}

#[test]
fn frames_concat_plus_remaining_equals_input() {
    // 故意切在事件中間 + emoji 中間的最壞 chunk 序列
    let raw = format!(
        "event: ping\ndata: {{}}\r\n\r\n{}data: 尾巴沒結束",
        String::from_utf8(event("🔥 你好")).unwrap()
    )
    .into_bytes();

    let mut splitter = SseByteSplitter::new();
    let mut reassembled = Vec::new();
    for chunk in raw.chunks(3) {
        for frame in splitter.feed_frames(chunk) {
            reassembled.extend(frame);
        }
    }
    reassembled.extend(splitter.take_remaining());
    // 一個 byte 都不准多、不准少、不准變
    assert_eq!(reassembled, raw);
}

#[test]
fn take_remaining_drains_buffer() {
    let mut splitter = SseByteSplitter::new();
    assert!(splitter.feed_frames(b"data: incomplete").is_empty());
    assert_eq!(splitter.take_remaining(), b"data: incomplete".to_vec());
    assert!(splitter.take_remaining().is_empty()); // 拿過就空了
}
