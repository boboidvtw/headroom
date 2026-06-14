"""headroom-lite — 跟著 REALIGNMENT 計劃親手重建的學習版本。

北極星原則（The North Star）
----------------------------
舊（錯）：壓縮 = 從對話歷史裡挑訊息丟掉。
新（對）：passthrough is sacred；只壓縮 live zone
         —— 型別感知、位置保留、舊 turn 一個 byte 都不動。

四鐵律：
  1. Cache 熱區永不可動（system / tools / 舊 turn / thinking 區塊）。
  2. Byte-faithful forwarding：沒被壓縮的內容以「原始 bytes」轉發。
  3. 只壓 live zone（最新 user / tool_result）+ token 驗證 + fallback。
  4. 可逆且狀態穩定。

學習里程碑：
  M0  byte-faithful passthrough proxy     （Phase A 之魂）✅
  M1  live-zone 壓縮                      （Phase B 之魂）✅
  M2  byte-level SSE 狀態機               （Phase C 之魂）✅
  M3  cache 穩定化                        （Phase E 之魂）✅
  M4  CCR 可逆取回                        （鐵律 4 / B7）✅
  M5  Rust port（rust-lite/）             （Phase C/I 之魂）✅
      Cow fallback、arbitrary_precision、跨語言 parity byte-for-byte
  M6  Rust port M3+M4（全 Rust pipeline + parity gate 腳本化）✅
  M7  axum HTTP proxy（串流轉發 + SSE boundary-preserving 重切）✅
  M8  lazy registration（有壓到才註冊 ccr_retrieve）✅

完整 pipeline（M8 lazy registration）：
  headroom_lite.pipeline.process_request(raw, store=store)
  順序：stabilize_request → compress_request →（有壓到才 register_ccr_tool）
  —— 多數請求不壓縮 → tools 全程不動 → 零 cache 影響。
"""

__version__ = "0.0.0"
