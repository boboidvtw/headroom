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

完整 pipeline（每請求、順序固定）：
  compress_request(stabilize_request(register_ccr_tool(raw)), store=store)
"""

__version__ = "0.0.0"
