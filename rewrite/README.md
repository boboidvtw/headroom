# headroom-lite — Learning Rebuild / 學習重建

> A from-scratch, TDD-driven rebuild of headroom's core, following the
> `REALIGNMENT/` plan (issues #1–#10). Python first, then Rust — mirroring
> the real plan's Phase A→C trajectory. The mature code in `crates/` and
> `headroom/` serves as the answer key; nothing outside `rewrite/` is touched.

> 跟著 `REALIGNMENT/` 計劃（issues #1–#10）親手從零重建 headroom 核心的
> 學習專案。Python 先行、再 port Rust —— 複製原計劃 Phase A→C 的真實演進。
> `crates/`、`headroom/` 的成熟程式碼是解答本；`rewrite/` 之外一概不動。

## North Star / 北極星

**Passthrough is sacred; only compress the live zone.**
Prompt cache matches byte-by-byte prefixes — any re-serialization
(`json.dumps` spacing, `\uXXXX` escapes, `1.50`→`1.5`, key reordering)
busts the cache from that byte onward. The real iron law is **determinism**:
clients resend full history every turn, so any transform must map the same
input bytes to the same output bytes, forever.

**Passthrough 神聖不可侵犯；只壓縮 live zone。**
prompt cache 靠逐字節前綴比對 —— 任何重新序列化都會讓 cache 從變動點
起全 miss。真正的鐵律是**確定性**：client 每輪重送完整歷史，所以任何
轉換都必須「同輸入 bytes → 永遠同輸出 bytes」。

## Milestones / 里程碑

| M | Commit | What / 內容 | Lesson / 核心一課 |
|---|---|---|---|
| M0 | `f146032` | byte-faithful passthrough proxy | `content=raw_bytes`, never re-serialize |
| M1 | `dba510b` | live-zone compression | deterministic, hash-keyed, fallback to original bytes |
| M2 | `e2534a3` | byte-level SSE state machine | buffer bytes, decode only complete events |
| M3 | `ac7f098` | cache stabilization | tool normalization; `dict ==` ignores key order (real bug caught) |
| M4 | `2a0f018` | CCR reversible retrieval | content-addressed store; register tool on EVERY request |
| M5 | `77b37c3` | Rust port + parity | `Cow` typed fallback; `arbitrary_precision`+`preserve_order`; Py/Rs SHA-256 identical |
| M6 | `200eba0` | Rust port of M3+M4 + full Rust pipeline + scripted parity gate | `Cow` relay: all-`Borrowed` ⇒ original bytes; serde_json `Value ==` ignores key order — compare serialized bytes; Python's `store.put` timing is hidden spec |
| M7 | `bf024e4` | axum HTTP proxy + streaming SSE re-chunking | boundary-preserving frames: `concat(frames)+remaining == input`; never hold a `MutexGuard` across `await`; `reqwest` without gzip feature = no silent decompression |
| M7.5 | `4b91dd5` | live-traffic validation + per-request observability line | vs real API: full cache hit on identical bodies (determinism proven live), −81.6% input tokens on big tool_results; **but** wrapping real Claude Code revealed `register_ccr_tool` mutating `tools` zeroes cross-process cache reads → M8 = lazy registration |
| M8 | `da29a03` | lazy registration — register CCR tool only when something compressed | decision moves from building block to orchestration layer; signal = compress returns same object (Py) / `Cow::Borrowed` (Rust); append CCR at tools **tail**, not sorted to front — prefix cache keeps client tools byte-identical, pushes divergence point later |
| M9 | `fd2e135` | CCR retrieve wired into the proxy — side-channel endpoint + server-side resolve loop | `POST /ccr/retrieve` exposes the store (200/404); the resolve loop intercepts the model's `ccr_retrieve` tool_use, serves the original from the store, and re-calls upstream until a real answer — the client never sees the injected tool; only fires on POST `/v1/messages` JSON responses, leaves SSE & plain JSON byte-faithful, capped at 8 hops; only intercepts when *every* tool_use is ccr_retrieve (foreign tools pass through untouched) |
| M10 | `d5c7220` | SSE streaming `ccr_retrieve` **observe-only** probe | deep-read of the answer key (`sse/anthropic.rs` + proxy `PR-C1`) revealed the industrial choice: streaming byte-passthrough is sacred, the SSE state machine is a passive telemetry tee (`mpsc` + spawned task, never blocks the client), retrieval honoring is **not** done in-stream. Faithful to that: `SseCcrProbe` detects a streamed `ccr_retrieve` call (track tool_use by `index`, accumulate `input_json_delta`, parse at `content_block_stop`) and logs an observability line — **zero bytes touched**. In-stream closure belongs to another layer; the non-stream path already closes the loop (M9) |

## Field Notes / 實測筆記 (2026-06-12)

Three live experiments against `api.anthropic.com` all passed (cache preserved,
81.6% input-token saving with unchanged answer quality, SSE intact). Wrapping a
real Claude Code session then exposed two things curl could not: Claude Code's
own `Agent` tool description is nondeterministic across processes, and adding
*any* tool (our CCR registration) forfeits the API's partial-cache resilience
for cross-process traffic. **M8 (`da29a03`) fixed this**: the CCR tool is now
registered lazily — only on requests that actually compressed something — so
most requests leave `tools` untouched (zero cache impact).

對真 `api.anthropic.com` 的三項實驗全過（cache 不破、input tokens 省 81.6%
且答案品質不變、SSE 完整）。接著 wrap 真的 Claude Code session，抓到 curl
抓不到的兩件事：Claude Code 自己的 `Agent` tool description 跨 process 不
確定；而我們在 tools 註冊 CCR 工具，會讓 API 對跨 process 流量的部分命中
容錯完全失效。**M8（`da29a03`）已治本**：CCR 工具改成 lazy 註冊 —— 只在
真的壓縮了的請求上註冊，多數請求 `tools` 全程不動（零 cache 影響）。

## Run / 執行

```bash
# Python (uv-managed 3.13 venv; fastapi doesn't support 3.14 yet)
cd rewrite && uv run pytest -q              # 37 tests

# Rust (standalone workspace)
cd rewrite/rust-lite && cargo test          # 52 tests

# Cross-language parity gate / 跨語言 parity gate（5 fixtures, byte-for-byte）
cd rewrite && ./scripts/parity.sh

# Run the Rust proxy / 跑 Rust proxy（M7；預設 127.0.0.1:8787 → api.anthropic.com）
cd rewrite/rust-lite && cargo run --example proxy_server
UPSTREAM=http://127.0.0.1:9999 PORT=8787 cargo run --example proxy_server  # override
```

## Pipeline

```python
# M8 lazy registration — stabilize → compress → register only if something compressed
# deterministic ∘ deterministic = deterministic; most requests leave tools untouched
headroom_lite.pipeline.process_request(raw, store=store)
```

```rust
// Rust equivalent — Cow lifetime relay; Borrowed only if no stage touched the bytes
pipeline::process_request(raw, Some(&mut store))
```

## CCR Retrieve / 取回 (M9)

Compression never loses data — the original is stashed in a content-addressed
store keyed by the `sha256:KEY` you see in the marker. Two ways to get it back:

- **Side-channel**: `POST /ccr/retrieve {"key":"..."}` → `200 {key,content}` or `404`.
- **Closed loop** (transparent): when the model calls the injected `ccr_retrieve`
  tool, the proxy serves the original from the store and re-calls upstream until a
  real answer comes back. The client never sees the injected tool. Runs only on
  `POST /v1/messages` JSON responses; SSE and plain JSON stay byte-faithful;
  foreign tool calls pass through untouched; capped at 8 hops.

壓縮永不丟資料 —— 原文存在 content-addressed store，key 就是標記裡的
`sha256:KEY`。兩種取回方式：

- **側信道**：`POST /ccr/retrieve {"key":"..."}` → `200 {key,content}` 或 `404`。
- **閉環**（透明）：模型呼叫注入的 `ccr_retrieve` 工具時，proxy 自己從 store
  取原文、重呼上游，直到拿到真答案 —— client 全程看不到這個工具。只在
  `POST /v1/messages` 的 JSON 回應上啟動；SSE 與普通 JSON 維持 byte-faithful；
  非 ccr_retrieve 的工具呼叫原樣放行；hop 上限 8。

### Streaming: observe-only / 串流：只觀察 (M10)

For SSE responses the proxy does **not** intercept — faithful to the answer key,
streaming bytes are sacred. `SseCcrProbe` passively detects a streamed
`ccr_retrieve` call and logs it; the byte stream is forwarded untouched. Closing
the loop mid-stream would require buffering (you can't un-send bytes), so it
belongs to a different layer — the non-stream path (above) is where closure lives.

對 SSE 回應，proxy **不**攔截 —— 忠於解答本，串流 bytes 神聖。`SseCcrProbe`
被動偵測串流裡的 `ccr_retrieve` 呼叫並記觀測線；byte 流原樣轉發。串流內閉環
得 buffer（送出去的 bytes 收不回來），屬於別層 —— 閉環在上面的非串流路徑。
