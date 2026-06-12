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

## Field Notes / 實測筆記 (2026-06-12)

Three live experiments against `api.anthropic.com` all passed (cache preserved,
81.6% input-token saving with unchanged answer quality, SSE intact). Wrapping a
real Claude Code session then exposed two things curl could not: Claude Code's
own `Agent` tool description is nondeterministic across processes, and adding
*any* tool (our CCR registration) forfeits the API's partial-cache resilience
for cross-process traffic. Next milestone: register the CCR tool lazily — only
on requests that actually compressed something.

對真 `api.anthropic.com` 的三項實驗全過（cache 不破、input tokens 省 81.6%
且答案品質不變、SSE 完整）。接著 wrap 真的 Claude Code session，抓到 curl
抓不到的兩件事：Claude Code 自己的 `Agent` tool description 跨 process 不
確定；而我們在 tools 註冊 CCR 工具，會讓 API 對跨 process 流量的部分命中
容錯完全失效。下一個里程碑：lazy registration —— 只在真的壓縮了的請求上
註冊工具。

## Run / 執行

```bash
# Python (uv-managed 3.13 venv; fastapi doesn't support 3.14 yet)
cd rewrite && uv run pytest -q              # 30 tests

# Rust (standalone workspace)
cd rewrite/rust-lite && cargo test          # 40 tests

# Cross-language parity gate / 跨語言 parity gate（4 fixtures, byte-for-byte）
cd rewrite && ./scripts/parity.sh

# Run the Rust proxy / 跑 Rust proxy（M7；預設 127.0.0.1:8787 → api.anthropic.com）
cd rewrite/rust-lite && cargo run --example proxy_server
UPSTREAM=http://127.0.0.1:9999 PORT=8787 cargo run --example proxy_server  # override
```

## Pipeline

```python
# deterministic ∘ deterministic = deterministic — the whole architecture in one line
compress_request(stabilize_request(register_ccr_tool(raw)), store=store)
```

```rust
// Rust equivalent — Cow lifetime relay; Borrowed only if no stage touched the bytes
pipeline::process_request(raw, Some(&mut store))
```
