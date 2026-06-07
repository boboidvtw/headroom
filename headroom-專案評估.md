# Headroom 專案評估報告

> 對象：`https://github.com/chopratejas/headroom`
> 評估日期：2026-06-07 ｜ 評估版本：v0.23.0（最後提交 2026-06-06，極活躍）
> 方法：完整 clone 後審讀 README / llms.txt / 套件設定 / 架構文件 / 原始碼結構 / 團隊內部審計（`REALIGNMENT/`）

---

## 1. 一句話定位

Headroom 是一個 **「AI agent 的 context 壓縮層」**——在 prompt、tool 輸出、log、RAG 結果、檔案、對話歷史送進 LLM **之前**先壓縮，宣稱「同樣的答案、用 60–95% 更少的 token」。核心賣點是 **local-first（資料留在本機）** 與 **reversible（可逆，原文不刪、模型可隨時取回）**。

授權為 **Apache 2.0**，可商用。

---

## 2. 它解決什麼問題

AI coding agent（Claude Code、Cursor、Codex、Aider 等）每次互動會把大量 tool 輸出、檔案內容、log 塞進 context window，造成：

- **Token 成本高**：重複、冗長的內容反覆送進模型。
- **Context window 爆掉**：長 session 撐不住。
- **冗餘雜訊稀釋訊號**：模型在一堆 JSON / log 裡找重點。

Headroom 的主張：用「內容感知壓縮」把這些東西縮小，但**保留語意**，且**可逆**（壓掉的原文存在本機，模型需要時用 `headroom_retrieve` 工具取回）。

---

## 3. 產品形態（4 種接入方式，同一條壓縮管線）

| 形態 | 用法 | 適合誰 |
|------|------|--------|
| **Library** | `from headroom import compress`（Python / TypeScript） | 想在自己 app 內嵌的人 |
| **Proxy** | `headroom proxy --port 8787`，零改碼、任何語言 | 想無痛插入現有 stack 的人 |
| **Agent wrap** | `headroom wrap claude\|codex\|cursor\|aider\|copilot` | 跑 coding agent 的人 |
| **MCP server** | `headroom_compress` / `headroom_retrieve` / `headroom_stats` | 任何 MCP client |

額外能力：**跨 agent 共享記憶**（Claude / Codex / Gemini 共用、自動去重）、**`headroom learn`**（挖掘失敗 session、把修正寫進 `CLAUDE.md` / `AGENTS.md`）。

---

## 4. 技術架構

### 4.1 壓縮管線（核心概念）

```
你的 agent / app
   │  prompts · tool 輸出 · logs · RAG 結果 · 檔案
   ▼
 Headroom（本機執行）
   CacheAligner → ContentRouter → CCR
                   ├─ SmartCrusher   (JSON 統計壓縮)
                   ├─ CodeCompressor (AST 感知，tree-sitter)
                   └─ Kompress-base  (純文字，HuggingFace 自訓模型)
   ▼
 LLM provider（Anthropic · OpenAI · Bedrock · …）
```

- **ContentRouter**：偵測內容型別，選對的壓縮器。
- **SmartCrusher / CodeCompressor / Kompress-base**：分別處理 JSON、程式碼 AST、散文。
- **CacheAligner**：穩定 prefix，讓 provider 的 KV cache 能命中。
- **CCR（Compress-Cache-Retrieve）**：原文存本機，可逆。

### 4.2 程式碼規模與語言（Python + Rust 混合）

| 指標 | 數值 |
|------|------|
| Python 檔案 / LOC | 776 檔 / ~30 萬行（含測試與部分 vendored） |
| Rust 檔案 / LOC | 175 檔 / ~6.6 萬行 |
| TS/JS 檔案 | 79 檔 |
| Python 測試檔 | 339 個 `test_*.py` |
| Rust crates | `headroom-core` / `headroom-proxy` / `headroom-parity` / `headroom-py` |

架構策略：Python 負責編排（orchestration）、CLI wrap、evals、learn、memory；Rust 負責高效能 proxy 與壓縮核心。**目前正進行 Python → Rust 的遷移**。

---

## 5. 成熟度與工程訊號（正面）

這專案的「外功」做得非常紮實，明顯不是玩具：

- **多通路發佈**：PyPI（`headroom-ai`）、npm（`headroom-ai`）、Docker（`ghcr.io`）、HuggingFace 模型（`kompress-base`）。
- **完整 CI/工程治理**：GitHub Actions、Codecov、`pre-commit`、`commitlint`、`release-please`（語意化版本）、`deny.toml`（Rust 供應鏈審查）、`.gitguardian.yaml`（密鑰掃描）、devcontainers（含 Qdrant / Neo4j 的 memory-stack）。
- **文件齊全**：獨立 docs 站、`llms.txt`/`llms-full.txt`（給 AI 讀的索引）、SECURITY.md、CONTRIBUTING.md、CODE_OF_CONDUCT.md、ENTERPRISE.md。
- **極度活躍**：v0.23.0，最後提交在評估前一天，CHANGELOG 持續更新。
- **基準測試透明且可重現**：`python -m headroom.evals suite --tier 1`。

### 宣稱的成效

**Token 節省（真實 agent 工作負載）：**

| 工作負載 | Before | After | 節省 |
|---------|-------:|------:|-----:|
| 程式碼搜尋（100 筆） | 17,765 | 1,408 | 92% |
| SRE 事故除錯 | 65,694 | 5,118 | 92% |
| GitHub issue 分類 | 54,174 | 14,761 | 73% |
| 程式庫探索 | 78,502 | 41,254 | 47% |

**準確度（標準 benchmark，宣稱壓縮後不掉分）：**

| Benchmark | N | Baseline | Headroom | Delta |
|-----------|--:|---------:|---------:|-------|
| GSM8K (數學) | 100 | 0.870 | 0.870 | ±0.000 |
| TruthfulQA (事實) | 100 | 0.530 | 0.560 | +0.030 |
| SQuAD v2 (QA) | 100 | — | 97% | 19% 壓縮 |
| BFCL (工具) | 100 | — | 97% | 32% 壓縮 |

> ⚠️ 這些數字來自專案自報，benchmark 各 N=100 屬小樣本。應視為「方向性證據」而非定論，建議親自跑 `headroom.evals` 在你自己的工作負載上驗證。

---

## 6. ⚠️ 最重要的發現：團隊自己的 `REALIGNMENT/` 審計

repo 內藏了一個 13 份文件的 `REALIGNMENT/` 目錄——**這是團隊用 10 個並行 deep-audit subagent 對自己做的坦白體檢**，內容遠比 README 誠實。誠實得令人敬佩，但也揭露了**目前架構的根本性問題**。摘錄其原文結論：

### 6.1 核心心智模型是「錯的」

> *"Headroom is built on the wrong mental model: **'compression means choosing what to drop from conversation history.'**"*
> — `REALIGNMENT/00-overview.md`

旗艦元件 **IntelligentContextManager (ICM)** 的做法是：把整個對話歷史 tokenize、對每則訊息打分、丟掉舊訊息直到符合預算。而它在 Rust proxy 上 `frozen_message_count: 0` 寫死——**意味著每次壓縮都從 index 0 開始丟訊息，把 Anthropic 的 prompt cache 打爆**（cache 命中率趨近 0%）。

正確模型應該相反：**「passthrough 是神聖的；只壓縮 live zone，型別感知、位置保留、用 side-channel 存 metadata」**；system prompt、tools、舊對話這些 cache 熱區**永遠不該動**。

### 6.2 審計揭露的問題（共 72 條，分 P0–P6）

| 等級 | 數量 | 性質 |
|------|-----:|------|
| **P0 cache-killer** | 7 | 打爆 prompt cache，**每個觸發的客戶都受影響** |
| P1 wire-format | 10 | SSE 串流解析缺 `thinking_delta`/`signature_delta`、UTF-8 跨封包切斷導致 emoji/中日韓字元掉字 |
| P2 over-build | 10 | **約 1 萬行架構過度設計**（ICM + scoring + relevance + rolling-window 等將被整批刪除）|
| P3 缺失基礎建設 | 9 | 缺 tool 陣列確定性排序、`cache_control` 自動放置、per-block token 驗證 |
| P4 long-tail + Bedrock | 12 | **Bedrock/Vertex 支援是「假的」**——透過 LiteLLM 有損轉換，會丟掉 `thinking`/`document`/`image` 等區塊 |
| P5 auth + 指紋 | 14 | `X-Headroom-*` header 外洩、subscription 模式有**指紋辨識/被撤銷風險**、OAuth token 明文存在記憶體 |
| P6 測試基建 | 10 | 缺 SHA-256 byte-faithful round-trip 測試、parity 比對器是 `Skipped` 樁 |

### 6.3 補救計畫

團隊規劃了 **9 階段、40 個 PR、約 13 週（並行約 8 週）** 的重寫（`REALIGNMENT/12-decisions-needed.md` 顯示 Phase A「今晚就上」的決策還在進行中）。計畫包含：刪掉 ~2.5 萬行跨兩語言的程式碼、把 proxy 改為 passthrough、Python forwarder 從 `httpx json=body`（會重新序列化破壞 byte 一致性）改為 `content=raw_bytes`、原生重建 Bedrock/Vertex 路徑等。

---

## 7. 綜合評估

### 7.1 優點

1. **問題真實且痛**：agent context 成本是當下熱門痛點（Trendshift 上榜）。
2. **接入方式靈活**：library / proxy / MCP / wrap 四選一，proxy 模式零改碼很有吸引力。
3. **可逆壓縮（CCR）是好設計**：不像競品直接丟資料，原文可取回降低了風險。
4. **工程治理一流**：CI、版本管理、安全掃描、文件、devcontainer 一應俱全。
5. **罕見的誠實**：`REALIGNMENT/` 這種把自己缺陷攤開的內部審計，反而是**團隊技術判斷力與成熟度**的強烈正面訊號。

### 7.2 風險與隱憂

1. **核心架構正在大重寫**：目前 release（v0.23.0）的旗艦壓縮路徑，依團隊自評會**嚴重破壞 Anthropic prompt cache**——諷刺的是，省下的 token 可能被 cache miss 的成本反噬。**現在採用要非常小心評估淨效益。**
2. **Bedrock/Vertex 名不副實**：若你的 stack 在 AWS/GCP，目前是有損轉換，會掉資料。
3. **Subscription 模式有合規/封號風險**：header 指紋可能讓 provider 偵測到 proxy（README 自己也標註 Copilot subscription 模式多平台「尚未完整驗證」）。
4. **體量龐大、複雜度高**：30 萬行 Python + 6.6 萬行 Rust + 雙語遷移中，學習與維運曲線陡。
5. **效益數字需自驗**：benchmark N=100、自報，未必對應你的工作負載。

### 7.3 適合 / 不適合

**適合：**
- 每天重度跑 coding agent、想省 token 又不想改碼（用 proxy 模式試水溫）。
- 需要跨多個 agent 共享記憶。
- 重視「壓縮可逆」勝過「壓縮極致」。

**先別碰 / 謹慎：**
- 生產環境且高度依賴 Anthropic prompt cache（等 Phase A/B 重寫落地後再評估）。
- 主力在 Bedrock / Vertex。
- 用 Copilot/OAuth subscription 且擔心封號合規。
- 只用單一 provider 的原生壓縮、也不需要跨 agent 記憶——那 README 自己都說「skip it」。

---

## 8. 給你的行動建議

1. **想試 → 從 proxy 模式 + 自己的真實工作負載開始**
   ```bash
   pip install "headroom-ai[all]"
   headroom proxy --port 8787      # 把 client 指向 127.0.0.1:8787
   headroom perf                   # 看實際節省
   ```
2. **務必盯 cache 命中率**：別只看 token 節省，要量「prompt cache hit rate」與「總成本」的淨變化（這正是 `REALIGNMENT` 警告的痛點）。
3. **追蹤重寫進度**：觀察 Phase A（passthrough lockdown）、Phase B（live-zone 引擎）相關 PR 是否合併。重寫完成前，把它當「有潛力但施工中」的工具。
4. **本機驗準確度**：拿你自己的任務跑 `python -m headroom.evals`，別只信 README 的 benchmark。

---

## 附錄：關鍵檔案索引

| 檔案 | 內容 |
|------|------|
| `README.md` | 對外定位、benchmark、相容矩陣 |
| `llms.txt` | 給 AI 讀的精簡索引 |
| `REALIGNMENT/00-overview.md` | **必讀**——團隊自評「錯誤心智模型」 |
| `REALIGNMENT/01-bug-list.md` | **必讀**——72 條 bug，P0–P6 分級，附 file:line 證據 |
| `REALIGNMENT/12-decisions-needed.md` | 重寫的待決策清單 |
| `Cargo.toml` / `pyproject.toml` | 依賴與 feature flags |
| `crates/` | Rust 核心（core / proxy / parity / py） |
| `headroom/` | Python 主套件 |
| `CHANGELOG.md` | 版本演進（release-please 自動產生） |

---

> **一句話總結**：Headroom 是一個**問題真實、工程治理一流、且罕見地誠實**的 context 壓縮專案，但它**目前的核心壓縮架構正因為會破壞 prompt cache 而進行大規模重寫**。值得高度關注與在沙盒裡實測，但若要上生產環境，請等 `REALIGNMENT` Phase A/B 落地、並務必親自量測「cache 命中率 + 總成本」的淨效益再決定。
