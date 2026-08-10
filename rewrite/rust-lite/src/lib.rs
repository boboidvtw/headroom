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
pub mod proxy;
pub mod strategies;
pub mod volatile;

pub mod live_zone {
    //! live-zone 壓縮引擎（與 Python 版行為 / 標記格式逐字對齊）。

    use std::borrow::Cow;

    use serde_json::Value;

    // content_key 的唯一真相來源在 ccr —— 標記與 store 永遠對得上
    use crate::ccr::CcrStore;

    pub const MIN_COMPRESSIBLE_BYTES: usize = 2048;

    /// 確定性截斷已移到 strategies dispatcher（M11）；保留薄委派以維持
    /// compress_block 的呼叫面不變。內容感知策略接進 STRATEGIES，這裡不必改。
    fn squeeze_text(text: &str) -> Option<String> {
        crate::strategies::squeeze_text(text)
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
            // frame 含邊界；事件視圖把邊界剝掉 —— 單一切割邏輯，兩種視圖
            self.split_frames(chunk)
                .into_iter()
                .map(|(frame, sep_len)| {
                    let mut event = frame;
                    event.truncate(event.len() - sep_len);
                    event
                })
                .collect()
        }

        /// M7：吞下一個 chunk，吐出「含原始邊界 bytes」的完整事件 frame。
        ///
        /// 與 `feed` 的差別：frame 是回程重組用的 —— 必須能逐字節還原，
        /// 所以 `\n\n` / `\r\n\r\n` 哪種結尾都原樣保留。
        /// 不變量：concat(所有 frames) + `take_remaining()` == 所有輸入。
        pub fn feed_frames(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
            self.split_frames(chunk)
                .into_iter()
                .map(|(frame, _)| frame)
                .collect()
        }

        /// 取走緩衝區剩餘 bytes（串流結束時的最後沖洗）。
        pub fn take_remaining(&mut self) -> Vec<u8> {
            std::mem::take(&mut self.buffer)
        }

        /// 共同核心：切出 (含邊界的 frame, 邊界長度) 序列。
        fn split_frames(&mut self, chunk: &[u8]) -> Vec<(Vec<u8>, usize)> {
            self.buffer.extend_from_slice(chunk);
            let mut frames = Vec::new();
            loop {
                // 找最早出現的事件邊界（兩種行尾都看）
                let earliest = BOUNDARIES
                    .iter()
                    .filter_map(|sep| find(&self.buffer, sep).map(|i| (i, sep.len())))
                    .min_by_key(|&(i, _)| i);
                let Some((cut, sep_len)) = earliest else {
                    return frames; // 沒有完整事件了，剩的繼續緩衝
                };
                frames.push((self.buffer[..cut + sep_len].to_vec(), sep_len));
                self.buffer.drain(..cut + sep_len);
            }
        }
    }

    use serde_json::Value;
    use std::collections::HashMap;

    /// M10 — SSE 串流裡的 ccr_retrieve **被動觀察**（observe-only）。
    ///
    /// 忠於工業版解答本（crates/headroom-proxy/src/sse/anthropic.rs）的選擇：
    /// byte-passthrough 神聖不可侵 —— 這個 probe 只「看」，一個 byte 都不碰。
    /// proxy 把每個上游 chunk 同時餵給它；它認出模型對 ccr_retrieve 的呼叫
    /// 就回報 key，proxy 拿去記觀測線。為什麼不在串流裡攔截取回？因為送出去
    /// 的 bytes 收不回來，要攔就得 buffer、那就毀了串流 —— 串流內閉環屬於別層。
    ///
    /// tool_use 在 Anthropic 串流裡按 `index` 分塊：content_block_start 帶
    /// type=tool_use 與 name；input 由 input_json_delta 的 partial_json 逐段
    /// 累積；到 content_block_stop 才湊齊、解析一次取出 key。
    #[derive(Default)]
    pub struct SseCcrProbe {
        splitter: SseByteSplitter,
        /// index → 正在累積的 tool_use 塊（只追 tool_use；text/thinking 略過）。
        blocks: HashMap<u64, ToolBlock>,
    }

    struct ToolBlock {
        name: String,
        partial_json: String,
    }

    impl SseCcrProbe {
        pub fn new() -> Self {
            Self::default()
        }

        /// 餵一個 chunk，回傳這次「剛在 content_block_stop 湊齊」的
        /// ccr_retrieve key 清單（多半為空 / 一個）。純觀察、不改 bytes。
        pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
            let mut keys = Vec::new();
            // 復用既有切割器：feed 回傳「剝掉邊界」的完整事件 bytes。
            for event_bytes in self.splitter.feed(chunk) {
                if let Some((name, data)) = parse_event(&event_bytes) {
                    self.apply(&name, &data, &mut keys);
                }
            }
            keys
        }

        fn apply(&mut self, event_name: &str, data: &[u8], keys: &mut Vec<String>) {
            let Ok(v) = serde_json::from_slice::<Value>(data) else {
                return; // 非 JSON data（理論上不會）→ 觀察者沉默放行
            };
            let index = v.get("index").and_then(Value::as_u64);
            match event_name {
                "content_block_start" => {
                    let Some(idx) = index else { return };
                    let cb = v.get("content_block");
                    let is_tool =
                        cb.and_then(|c| c.get("type")).and_then(Value::as_str) == Some("tool_use");
                    if is_tool {
                        let name = cb
                            .and_then(|c| c.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        self.blocks.insert(idx, ToolBlock { name, partial_json: String::new() });
                    }
                }
                "content_block_delta" => {
                    let Some(idx) = index else { return };
                    let Some(block) = self.blocks.get_mut(&idx) else { return };
                    let delta = v.get("delta");
                    if delta.and_then(|d| d.get("type")).and_then(Value::as_str)
                        == Some("input_json_delta")
                    {
                        if let Some(p) =
                            delta.and_then(|d| d.get("partial_json")).and_then(Value::as_str)
                        {
                            block.partial_json.push_str(p);
                        }
                    }
                }
                "content_block_stop" => {
                    let Some(idx) = index else { return };
                    // 塊結束才結算：是 ccr_retrieve 且 input 解得出 key 才回報。
                    if let Some(block) = self.blocks.remove(&idx) {
                        if block.name == "ccr_retrieve" {
                            if let Ok(input) = serde_json::from_str::<Value>(&block.partial_json) {
                                if let Some(key) = input.get("key").and_then(Value::as_str) {
                                    keys.push(key.to_owned());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// 把一段「剝掉邊界」的 SSE 事件 bytes 解析成 (event_name, data)。
    /// 多個 data: 行依 WHATWG SSE 規範以 `\n` 串接；找不到 data: 回 None。
    fn parse_event(block: &[u8]) -> Option<(String, Vec<u8>)> {
        let mut event_name: Option<String> = None;
        let mut data_parts: Vec<&[u8]> = Vec::new();
        for raw_line in block.split(|&b| b == b'\n') {
            // 容忍 CRLF：剝掉行尾單一 \r
            let line = match raw_line.strip_suffix(b"\r") {
                Some(l) => l,
                None => raw_line,
            };
            if line.is_empty() || line[0] == b':' {
                continue; // 空行 / 註解（: ping）跳過
            }
            let (field, value) = match line.iter().position(|&b| b == b':') {
                Some(p) => (&line[..p], &line[p + 1..]),
                None => (line, &line[line.len()..]),
            };
            // 規範：值前單一空白要剝掉
            let value = value.strip_prefix(b" ").unwrap_or(value);
            match field {
                b"event" => event_name = std::str::from_utf8(value).ok().map(str::to_owned),
                b"data" => data_parts.push(value),
                _ => {}
            }
        }
        let name = event_name?;
        if data_parts.is_empty() {
            return None;
        }
        Some((name, data_parts.join(&b'\n')))
    }
}
