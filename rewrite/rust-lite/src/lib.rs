//! headroom-lite-rs — 學習重建第二幕：Python 核心 port 到 Rust。
//!
//! 北極星不變：passthrough is sacred；只壓縮 live zone。
//! Rust 帶來的升級：
//!   - `Cow<[u8]>` 把 fallback 契約寫進型別 —— `Borrowed` 就是
//!     「原 bytes 本人，連碰都沒碰」的編譯期證明。
//!   - serde_json `arbitrary_precision` + `preserve_order`：
//!     數字字面值與 key 順序在 parse → serialize 之間不變
//!     （沒有這兩個 feature，1.50 會變 1.5、key 會被重排 —— cache 炸）。

pub mod cache_stabilization;
pub mod ccr;
pub mod pipeline;

pub mod live_zone {
    //! live-zone 壓縮引擎（與 Python 版行為 / 標記格式逐字對齊）。

    use std::borrow::Cow;

    use serde_json::Value;

    // content_key 的唯一真相來源在 ccr —— 標記與 store 永遠對得上
    use crate::ccr::{content_key, CcrStore};

    pub const MIN_COMPRESSIBLE_BYTES: usize = 2048;
    pub const HEAD_LINES: usize = 20;
    pub const TAIL_LINES: usize = 10;

    /// 確定性截斷：頭 + 標記 + 尾。行數不夠回 None。
    /// 標記格式必須與 Python 版逐字相同 —— 跨語言 parity 的前提。
    fn squeeze_text(text: &str) -> Option<String> {
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() <= HEAD_LINES + TAIL_LINES {
            return None;
        }
        let omitted = lines.len() - HEAD_LINES - TAIL_LINES;
        let marker = format!(
            "[... headroom-lite squeezed {omitted} lines | sha256:{} ...]",
            content_key(text)
        );
        let mut parts: Vec<&str> = Vec::with_capacity(HEAD_LINES + TAIL_LINES + 1);
        parts.extend(&lines[..HEAD_LINES]);
        parts.push(&marker);
        parts.extend(&lines[lines.len() - TAIL_LINES..]);
        Some(parts.join("\n"))
    }

    /// 入口：live-zone 壓縮（不收存原文 —— M1/M5 的原始行為）。
    pub fn compress_request(raw: &[u8]) -> Cow<'_, [u8]> {
        compress_request_with_store(raw, None)
    }

    /// 入口：live-zone 壓縮 + 可選 CCR store（M4 整合）。
    ///
    /// 回傳 `Cow::Borrowed(raw)` = 沒事可做，原 bytes 本人；
    /// 回傳 `Cow::Owned(..)` = 真的壓到了，規範化重新序列化。
    /// 任何失敗一律 Borrowed 放行 —— 壓縮永遠不准弄壞請求。
    /// store 給了就在壓縮前收存原文 —— 永不丟資料。
    pub fn compress_request_with_store<'a>(
        raw: &'a [u8],
        mut store: Option<&mut CcrStore>,
    ) -> Cow<'a, [u8]> {
        let Ok(mut body) = serde_json::from_slice::<Value>(raw) else {
            return Cow::Borrowed(raw);
        };

        let mut changed = false;
        if let Some(last) = body
            .get_mut("messages")
            .and_then(Value::as_array_mut)
            .and_then(|m| m.last_mut())
        {
            // live zone 定義與 Python 版一致：最後一則、user、block list
            if last.get("role").and_then(Value::as_str) == Some("user") {
                if let Some(blocks) = last.get_mut("content").and_then(Value::as_array_mut) {
                    for block in blocks {
                        // 先算好結果再寫回 —— 借用檢查器強迫我們把
                        // 「讀」與「寫」分離，正好就是不可變的紀律。
                        let squeezed = match block.get("content").and_then(Value::as_str) {
                            Some(text)
                                if block.get("type").and_then(Value::as_str)
                                    == Some("tool_result")
                                    && text.len() >= MIN_COMPRESSIBLE_BYTES =>
                            {
                                let result = squeeze_text(text);
                                // 與 Python 版對齊：行數門檻通過就先收存原文
                                //（即使下面的「沒賺就不動」fallback 之後放棄壓縮）
                                if result.is_some() {
                                    if let Some(s) = store.as_deref_mut() {
                                        s.put(text);
                                    }
                                }
                                result.filter(|s| s.len() < text.len())
                            }
                            _ => None,
                        };
                        if let (Some(s), Some(obj)) = (squeezed, block.as_object_mut()) {
                            // preserve_order 的 Map：insert 既有 key 保留原位置
                            obj.insert("content".into(), Value::String(s));
                            changed = true;
                        }
                    }
                }
            }
        }

        if !changed {
            return Cow::Borrowed(raw);
        }
        match serde_json::to_vec(&body) {
            Ok(bytes) => Cow::Owned(bytes),
            Err(_) => Cow::Borrowed(raw), // 失敗模式契約：序列化失敗原樣放行
        }
    }
}

pub mod sse {
    //! byte-level SSE splitter（與 Python 版行為一致）。
    //! 緩衝 bytes、只在完整事件邊界（空行）切開 —— 被切半的
    //! UTF-8 字元安全地躺在緩衝區，從頭到尾不 decode。

    const BOUNDARIES: [&[u8]; 2] = [b"\r\n\r\n", b"\n\n"];

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    #[derive(Default)]
    pub struct SseByteSplitter {
        buffer: Vec<u8>,
    }

    impl SseByteSplitter {
        pub fn new() -> Self {
            Self::default()
        }

        /// 吞下一個 chunk，吐出「此刻已完整」的事件 bytes。
        pub fn feed(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
            self.buffer.extend_from_slice(chunk);
            let mut events = Vec::new();
            loop {
                // 找最早出現的事件邊界（兩種行尾都看）
                let earliest = BOUNDARIES
                    .iter()
                    .filter_map(|sep| find(&self.buffer, sep).map(|i| (i, sep.len())))
                    .min_by_key(|&(i, _)| i);
                let Some((cut, sep_len)) = earliest else {
                    return events; // 沒有完整事件了，剩的繼續緩衝
                };
                events.push(self.buffer[..cut].to_vec());
                self.buffer.drain(..cut + sep_len);
            }
        }
    }
}
