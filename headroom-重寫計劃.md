# Headroom 重寫計劃（Rewrite Plan）

> 基於 `REALIGNMENT/` 內部審計（10 個並行 deep-audit subagent 結論）整理成的**可執行重寫路線圖**。
> 制定日期：2026-06-07 ｜ 對應版本起點：v0.23.0
> 本文件是「綜合版作戰計劃」，把 72 條 bug、9 階段、40 PR 收斂成清楚的執行順序、驗收標準與風險控管。

---

## 0. 北極星原則（The North Star）

整個重寫只圍繞**一句心智模型的翻轉**：

> ❌ 舊（錯）：「壓縮 = 從對話歷史裡挑訊息丟掉」
> ✅ 新（對）：「**passthrough 是神聖的；只壓縮 live zone**——型別感知、位置保留、hash-keyed、用 side-channel 存 metadata」

四條不可違背的鐵律：

1. **Cache 熱區永不可動**：system prompt、tools 陣列、舊對話 turn、`thinking`/`redacted_thinking`/`compaction` 區塊——**一個 byte 都不能改**。
2. **Byte-faithful forwarding**：沒被壓縮的內容必須以**原始 bytes** 轉發上游，不可經過 `json.dumps` / `serde_json::Value` 重新序列化（否則破壞 prompt cache）。
3. **只壓 live zone**：只動「最新 user 訊息內容 + 最新 tool_result + 最新 function_call_output」，且每次壓縮都做 **token 驗證 + fallback**。
4. **可逆且狀態穩定**：CCR 的 `ccr_retrieve` 工具**每次請求都註冊**（不可時有時無，否則 tools 陣列變動 → cache bust）。

---

## 1. 全局時程與依賴

| 階段 | 名稱 | 工期 | 可並行？ | 核心產出 |
|------|------|------|----------|----------|
| **A** | Lockdown（止血） | 1 週 | 序列（最先） | proxy 改 passthrough，停止 cache 流血 |
| **B** | Live-zone 引擎 | 2 週 | 依賴 A | 刪 ~1 萬行 ICM，建 live-zone 壓縮器 |
| **C** | Rust proxy 路徑 | 3 週 | 依賴 B | byte-level SSE parser + 三大 endpoint |
| **D** | Bedrock/Vertex 原生 | 2 週 | 依賴 C | 刪 LiteLLM 有損轉換，原生簽章路徑 |
| **E** | Cache 穩定化 | 1 週 | 依賴 B | tool 排序、`cache_control` 自動放置 |
| **F** | Auth-mode 政策 | 1 週 | 依賴 C | 三模式分流 + 反指紋 |
| **G** | RTK + 可觀測性 | 1 週 | 依賴 C | 指標、wrap CLI 擴充 |
| **H** | Python 退役 | 2 週 | 依賴 B+C | 刪舊 Python proxy/handlers |
| **I** | 測試基建 | 持續並行 | 全程 | round-trip 測試、parity gate |

**總計**：序列約 **13 週**，並行約 **8 週**。

```
A ──► B ──┬──► C ──┬──► D
          │        ├──► F
          ├──► E   └──► G
          └──► H（需 C）
I ════════════════════════► （全程並行）
```

---

## 2. 各階段執行細節

### Phase A — Lockdown：今晚就止血（1 週，7 PR）

**目標**：在不重寫核心的前提下，讓 prompt cache 立刻停止被打爆。

| PR | 任務 | 對應 bug | 驗收標準 |
|----|------|----------|----------|
| **A1** | `/v1/messages` 壓縮改為 passthrough（停止呼叫 ICM） | P0-3/4/5 | 小 diff（約 -180/+30）；ICM 不再從 index 0 丟訊息 |
| **A2** | 移除 system prompt 的 `.strip()` + memory append；刪 CacheAligner rewrite path（~400 行） | P0-1, P2-23 | system prompt byte 不變；memory 改注入 live zone |
| **A3** | 所有 Python forwarder：`httpx json=body` → `content=raw_bytes` | P0-2 | 出站 bytes 與入站 byte-equal（除非確實壓縮） |
| **A4** | Rust：尊重客戶 `cache_control` marker；`serde_json` 啟用 `arbitrary_precision` + `raw_value` | P0-3/5, P4-46 | `frozen_message_count` 依最高 cache_control index；`1.0` 不再變 `1` |
| **A5** | 出站 header 剝除 `x-headroom-*` | P5-49 | 上游收不到任何 Headroom 自訂 header |
| **A6/A7** | memory tool 注入改 **session-sticky**；`anthropic-beta` 順序固定、session 黏著 | P0-6 | session 中途不再 flicker tools / beta |
| **A8** | Python hotfix 包：SSE 補 delta 類型、UTF-8 bytes 緩衝、`phase` 欄位修復、加 byte-faithful round-trip 測試 | P0-7, P1-8/9 | SHA-256 round-trip 測試通過 |

**風險**：A1 讓壓縮暫時消失（功能倒退），但 cache 命中率立即回升，**淨成本為正**。Phase B 把壓縮以正確方式加回。

---

### Phase B — Live-zone 引擎（2 週，7 PR）

**目標**：刪掉錯誤架構，建立正確的 live-zone-only 壓縮。

**B1 — 大刪除（~1 萬行）**：
- 刪 `intelligent_context.py`、`manager.rs`、`icm.rs` 及 proxy 呼叫點（ICM 本體）
- 刪 `RollingWindow`(395行)、`ProgressiveSummarizer`(508行)、`scoring.py`(459行)、`tool_crusher.py`
- 刪 `crates/headroom-core/src/{scoring,relevance}/`（~3100行）、大部分 `context/`
- **保留** `safety.rs`（tool-pair 原子性）→ 移到 `transforms/safety.rs`

**B2-B7 — 建新引擎**：

| PR | 任務 | 驗收標準 |
|----|------|----------|
| B2 | Rust live-zone block dispatcher：只對最新 user content / tool_result / function_call_output 跑壓縮 | 舊 turn 完全不被觸碰 |
| B3 | 接上 per-type 壓縮器：SmartCrusher / LogCompressor / DiffCompressor / SearchCompressor / Kompress | 各型別正確路由 |
| B4 | **每個 block 做 token 驗證 + fallback**；per-content-type byte 門檻（code>2KB, JSON>1KB, log>500B, text>5KB） | 壓縮後 token 不增加，否則退回原文 |
| B5 | TOIN 改為**嚴格觀察模式**（不再 request-time 影響決策） | 相同輸入 bytes → 相同輸出 |
| B6 | memory 注入 refactor：移出 request lifecycle，改為客戶顯式呼叫的工具 | 不再每 turn 非確定性 prepend |
| B7 | CCR 強化：持久化 backend + `ccr_retrieve` **每請求都註冊** + marker 寫進 block side-channel | tools 陣列不因壓縮與否而變動 |

---

### Phase C — Rust proxy 路徑（3 週，5 PR）

**目標**：把 proxy 主力從 Python 搬到 Rust，建完整 SSE 狀態機。

| PR | 任務 | 對應 bug |
|----|------|----------|
| C1 | **byte-level SSE parser**（完整狀態機）：全 delta 類型、`error` 事件、truncation 偵測、index-keyed block map | P1-8/9/14/15/17, P4-48 |
| C2 | `/v1/chat/completions` Rust handler（含 `refusal` 欄位） | P1-16 |
| C3 | `/v1/responses` Rust handler（HTTP + streaming） | P1-12, P4-42 |
| C4 | `/v1/conversations` 盲區處理 | P4-40 |
| C5 | per-item-type passthrough 保留：V4A patch、`local_shell_call` argv、Codex `phase`、MCP items、`compaction` | P0-7, P4-47 |

**核心要求**：每種 item 型別都要**明確**保留（目前很多是「catch-all 碰巧沒壞」），加 log + 測試。

---

### Phase D — Bedrock/Vertex 原生（2 週，4 PR）

**目標**：刪掉「假的」LiteLLM 有損轉換，建原生路徑。

- **D1**：刪 `headroom/backends/litellm.py`（會丟 `thinking`/`document`/`image` 等區塊）
- **D2/D3**：原生 AWS Bedrock `/model/.../invoke`（SigV4 簽章）
- **D4**：原生 GCP Vertex `streamRawPredict`（ADC 簽章）

**驗收**：Bedrock/Vertex 流量的 cache fidelity 與 Anthropic 直連一致；`thinking` 等區塊零損失。

---

### Phase E — Cache 穩定化（1 週，6 PR）

| PR | 任務 |
|----|------|
| E1 | Rust path 加 tool 陣列確定性排序 |
| E2 | JSON Schema keys 遞迴排序 |
| E3 | Anthropic：自動放置最多 4 個 `cache_control` breakpoint |
| E4 | OpenAI：自動注入 `prompt_cache_key` |
| E5 | volatile-content 偵測器 + 客戶警告（**只警告、不 rewrite**） |
| E6 | cache-bust drift 遙測（跨請求 prefix-hash 漂移偵測） |

---

### Phase F — Auth-mode 政策（1 週，4 PR）

**目標**：PAYG / OAuth / subscription 三模式分流，消除指紋/封號風險。

- **F1**：`classify_auth_mode(headers)` → `payg | oauth | subscription`
- **F2**：per-mode 壓縮政策 gate；subscription 模式保留 `accept-encoding`、不自動注入 `OpenAI-Beta`
- **F3**：subscription tracker 只存 token **hash + ID**（不存明文 bearer）；TOIN 聚合 key 改為 `(auth_mode, model_family, structure_hash)` 防跨租戶洩漏
- **F4**：`X-Forwarded-*` header 依 auth mode 條件加入

---

### Phase G — RTK + 可觀測性（1 週，3 PR）

- **G1**：擴充 wrap CLI（cline、continue、goose、openhands）
- **G2**：接上死掉的 `tokens_saved_rtk` 欄位
- **G3**：Prometheus 指標——per-session cache-hit-rate、per-block 壓縮比直方圖、rate-limit header 觀測、`service_tier` 記錄、image base64 log 遮蔽

---

### Phase H — Python 退役（2 週）

刪除：`proxy/server.py`、所有 handlers、`responses_converter.py`、`memory_handler.py`、`batch.py`、`semantic_cache.py`、`transforms/*`（Python）。

**保留**：CLI wrappers、RTK installer、evals、`learn`、memory writers、tokenizers、TOIN。

---

### Phase I — 測試基建（全程並行）

| 項目 | 內容 |
|------|------|
| Round-trip | 用真實 production payload 做 SHA-256 byte-faithful 測試 |
| SSE fixtures | UTF-8 切斷、ping、所有 delta 類型、`[DONE]`、mid-stream error |
| Property tests | SSE parser 不 panic、壓縮後 token 非遞增 |
| Parity | 把 `ccr`/`log_compressor`/`cache_aligner` 的 `Skipped` 樁升級為真比對器 |
| Shadow test | 真實流量比對 Python vs Rust 輸出 byte-for-byte |
| CI gate | `make test-parity` 從 nightly `continue-on-error` 改為 **per-PR gate**（`Diff` 直接 fail） |
| 指標 | per-session cache-hit-rate 連續監控 |

---

## 3. 優先級總覽（72 條 bug 對應階段）

| 優先級 | 數量 | 性質 | 落在 |
|--------|-----:|------|------|
| P0 cache-killer | 7 | 打爆 cache，每客戶受影響 | Phase A |
| P1 wire-format | 10 | SSE / 串流損壞 | A + C |
| P2 over-build | 10 | 架構過度設計（待刪） | B |
| P3 missing infra | 9 | 缺 cache 穩定化基建 | E |
| P4 long-tail + Bedrock | 12 | Bedrock 假支援 + OpenAI 長尾 | C + D |
| P5 auth + 指紋 | 14 | 安全 / 封號風險 | F + G |
| P6 test infra | 10 | 測試缺口 | I（並行） |

---

## 4. 關鍵決策點（開工前需拍板）

> 摘自 `REALIGNMENT/12-decisions-needed.md`，fork 維護者需逐項 greenlight：

1. **Phase A 是否今晚就上？**　建議：**A1 今晚上**（小 diff，立即止血），A2–A8 本週內陸續落地。
2. **ICM 刪除範圍？**　建議：Tier 1（ICM 本體）+ Tier 2（只服務 ICM 的 scoring/relevance/rolling-window 等）一起刪，約 1 萬行。
3. **MessageScorer 的 Rust port 刪不刪？**　建議：**刪**。其唯一消費者 `DropByScoreStrategy` 將退役，留著只是維護負擔（PR #338/#343 屬沉沒成本）。

---

## 5. 給 fork 維護者的執行建議

1. **先做 Phase A 就有 80% 價值**：止血後 cache 命中率回升，淨成本立刻改善，風險最低。
2. **每個 PR 都掛 byte-faithful round-trip 測試**：這是整個重寫的安全網，沒有它一切免談。
3. **量測北極星指標**：不是「省多少 token」，而是「**prompt cache hit rate + 總成本**」的淨變化。
4. **保留可逆性**：CCR 是這專案相對其他競品的差異化優勢，重寫中務必維持「原文不刪、可取回」。
5. **分支策略**：每階段開 feature branch，Phase A 因為止血急迫可考慮快速合併，B 之後務必經 parity gate 才 merge。

---

## 附錄：原始審計文件對照

| 本計劃章節 | 原始來源 |
|-----------|----------|
| §0 北極星原則 | `REALIGNMENT/00-overview.md` |
| §2 各階段細節 | `REALIGNMENT/03~11-phase-*.md` |
| §3 bug 優先級 | `REALIGNMENT/01-bug-list.md`（72 條，附 file:line 證據） |
| §2 架構藍圖 | `REALIGNMENT/02-architecture.md` |
| §4 決策點 | `REALIGNMENT/12-decisions-needed.md` |

> **一句話總結**：先用 **Phase A 止血**（今晚就能上、風險最低、價值最高），再用 **Phase B 把錯誤的 ICM 架構換成 live-zone 壓縮**，其餘階段補齊 Rust 化、Bedrock 原生、cache 穩定化與安全。整條路線的安全網是 **byte-faithful round-trip 測試**，成功指標是 **cache 命中率 + 總成本的淨改善**，而非單看 token 數字。
