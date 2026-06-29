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
| M11 | `4e10def` | pluggable content-sniffing compression strategy dispatcher (skeleton) | pure refactor of live-zone — `Strategy = (applies, squeeze)`, `TRUNCATE` as catch-all; adding log/search/diff later = one strategy + register; `store.put` timing stays inside the strategy (sacred spec); Py/Rs store-side asymmetry but markers byte-identical ⇒ parity holds; proven byte-identical vs pre-refactor across 92 inputs + cross-language differential + adversarial fuzz |
| M12 | `4020ee2` | first real content-aware strategy: **log compression** | classify lines by severity, drop noise (`TRACE/DEBUG/INFO`), keep signal (`WARN/ERROR/FATAL/CRITICAL`) + others; marker = `dropped N log lines` + `content_key`. Beats blind head/tail truncation on the thing that matters: ERRORs buried in the **middle** — truncate keeps only head+tail and loses them, log sniffs every line and keeps each one (content-aware trade: keep errors over raw byte ratio). Claims only when noise ≥ 30% (else falls through to truncate, so all-error logs aren't swallowed). Whole-word match (`INFORMATION ≠ INFO`), byte-level ASCII boundaries, Py/Rs byte-identical. Existing `01_messy_full` (a real log) auto-routes to log: 14473→**1147** (truncate was 2524) — smaller *and* signal-preserving; new `06_noisy_log` fixture locks the keep-middle-errors branch |
| M13 | _local_ | second content-aware strategy: **diff compression** | classify unified/git-diff lines by role, drop unchanged context (` ` space-prefixed), keep every structural line: hunk headers (`@@`), file headers (`diff`/`index`/`---`/`+++`), and all `+`/`-` changes; marker = `dropped N diff context lines` + `content_key`. Same win as log on buried signal: changes scattered in a large context block — truncate keeps only head+tail and loses the middle ones, diff sniffs every line and keeps each change (hunk headers already encode line ranges; CCR store holds the original for reversal). Claims only with a hunk header present (so markdown `+`/`-` bullets aren't mistaken for a diff) and context ≥ 30% (else falls through to truncate). Registered **before** log — a `@@`-bearing diff's structure outranks log severity classification (existing logs have no hunk header, so they don't collide). Pure ASCII byte prefixes (no `strip`/`trim` unicode divergence), Py/Rs byte-identical. New `07_diff.json` fixture: 7227→**1857** bytes through the full pipeline, cross-language byte-for-byte |
| M14 | _local_ | third content-aware strategy: **search compression** | grep/rg `file:lineno:content` output — cap matches per file (keep first 3, drop the rest), marker = `dropped N search result lines` + `content_key`. The teaching landmine: detecting a match line by "`:`-split, second field all digits" also matches log timestamps (`10:30:45` is literally `field:digits:field`) — so search would cannibalize logs. Fix: require the file_key (before the first `:`) to contain a `/`; real `grep -rn pat .` always carries a path (`./src/foo.py:12:`), a timestamp prefix (`2026-06-20T10`) never does — a pure ASCII byte check, identical across languages. Registered `(DIFF, SEARCH, LOG, TRUNCATE)`: grep-over-logs routes to search, but existing logs have no `/`-bearing `file:line:` lines so they still route to log (no M12 regression). Determinism via in-order scan + per-file counts (HashMap looked up, never iterated → no hash-order dependence). New `08_search.json` fixture: 4289→**1702** bytes, byte-for-byte |
| M15 | _local_ | fourth content-aware strategy: **json compression** | find the largest JSON array, keep first 5 + last 2 elements, splice a marker string element in between (result stays valid JSON), copy everything outside the array verbatim; marker = `dropped N array elements` + `content_key`. The headline lesson is the parity insight: the obvious approach — parse, truncate, re-serialize — hits the number-reserialization landmine (`json.dumps` normalizes `1.10`→`1.1`, but Rust's `arbitrary_precision` keeps `1.10` → divergence). The fix is to **never re-serialize any value**: a byte-level structural scan (tracking string literals and nesting depth) records element byte-spans, and kept elements are copied as raw byte slices — only the structural chars (`[` `,` `]`) and the marker are newly written, all ASCII constants. So Python and Rust can't diverge on values. Determinism: tie-break is most-elements-then-smallest-start (Python `max` keeps first on ties, Rust `max_by_key` keeps last, so both use explicit "strictly-greater replaces"). Guarded to genuine JSON documents (first non-whitespace byte is `[`/`{`) so bracket-containing prose/logs aren't mistaken for JSON. Registered first. New `09_json.json` fixture deliberately includes `1.10` and `1e10`: 2959→**1651** bytes, byte-for-byte across languages with the tricky numbers preserved verbatim |
| M16 | _local_ | fifth content-aware strategy: **stack trace compression** | segment a stack trace into frames (Python `File "..."`, Java/JS `at ...(`), keep the first 3 + last 3 frames, drop the middle frames (with their continuation lines), keep **every non-frame line** (the `Traceback` header, the final `XxxError: msg`, chained-exception separators); marker = `dropped N stack frames` + `content_key`. Beats blind truncation on frame integrity: truncate cuts at a line boundary and can split a `File "..."` header from its code line, and a multi-line trailing message can push the crucial error line out of the tail window — the frame-aware strategy never splits a frame and always keeps the message lines. A frame header is detected byte-level (strip leading `0x20`/`0x09`, then `File "` or `at ` + `(`) — the `(` requirement rejects prose like "at the store". Registered `(JSON, DIFF, SEARCH, LOG, STACKTRACE, TRUNCATE)` — just before TRUNCATE, so every existing fixture is claimed by an earlier strategy first (zero regression by construction; a pure stack trace has no `INFO/DEBUG` noise so log never claims it). Determinism via in-order frame segmentation + index math (no hash-order dependence), Py/Rs byte-identical. New `10_stacktrace.json` fixture (30-frame recursion trace): 2906→**1386** bytes through the full pipeline, cross-language byte-for-byte |
| M17 | `b6fa1a0` | sixth content-aware strategy: **CSV/table compression** | keep the header row + first 3 + last 2 data rows, collapse the middle homogeneous rows into a single marker (`dropped N table rows` + `content_key`). Beats blind truncation on semantics: truncate doesn't know "the header is the signal" — once data rows are numerous enough to push the column names out of the `HEAD_LINES` window the names are lost and the remaining number rows are unreadable; the CSV strategy pins the header to line 0. Detection is conservative/strong-signal: after dropping a single trailing newline, **every** non-empty line must contain the same delimiter (`,` preferred, then `\t`) the same number of times (≥1) — prose can't have an identical comma count on every line; an interior blank line or a quoted-comma (which breaks the equal-count invariant) falls through to truncate. Delimiter counting is pure ASCII byte counting (`,`=0x2C/`\t`=0x09 are never UTF-8 continuation bytes) ⇒ identical to Python `str.count`. Registered `(JSON, DIFF, SEARCH, LOG, STACKTRACE, CSV, TRUNCATE)` — just before the catch-all, so every existing fixture is claimed earlier (zero regression: all 10 prior fixtures keep their exact byte counts). New `11_csv.json` fixture (60-row table): 3533→**1095** bytes through the full pipeline, cross-language byte-for-byte |

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

## Notes — M11 (2026-06-19)

M11 was a **pure refactor**: `live_zone` went from hardcoded head/tail truncation to
a pluggable content-sniffing dispatcher (`Strategy = applies + squeeze`, `TRUNCATE`
as catch-all). Verified far beyond the unit tests — behavior-equivalence vs the
pre-M11 version across **92 diverse inputs** (byte-identical + CCR store identical),
a **cross-language differential** (Python vs Rust `compress_stdin`, zero diff), and
**adversarial** malformed inputs (zero crashes, all passthrough).

A meta-lesson surfaced: the session that built M11 ran in a corrupted sandbox that
*rolled back commits and faked success reports* — phantom pushes, a non-existent
`OPTIMIZATION.md`, a README "M11 update" that was never actually written. The fix
mirrored this project’s own North Star: when the environment lies, trust only
**ground truth** — directly-run commands (`git log`, `parity.sh` exit code) and the
compiler, never wrapped queries. M11’s code landed only because it was rebuilt via a
self-contained script in a clean terminal, then verified from scratch.

## Notes — M11（2026-06-19）

M11 是**純重構**：`live_zone` 從寫死的頭尾截斷，改為可插拔的 content-sniffing
dispatcher（`Strategy = applies + squeeze`、`TRUNCATE` 殿後 catch-all）。驗證遠超
單元測試——對 M11 重構前版本做 **92 個多樣輸入**的行為等價（byte-identical +
CCR store 一致）、**跨語言 differential**（Python vs Rust `compress_stdin`，零差異）、
**對抗性**畸形輸入（零崩潰、全 passthrough）。

還浮現一個 meta 教訓：建造 M11 的 session 跑在會**回滾 commit、偽造成功報告**的損壞
沙箱裡——幻象 push、不存在的 `OPTIMIZATION.md`、從未真正寫入的 README「M11 更新」。
解法呼應了專案自己的 North Star：環境說謊時，只信 **ground truth**——直跑命令
（`git log`、`parity.sh` 的 exit code）與編譯器，絕不信包裝過的查詢。M11 的程式碼能
落地，是因為用自包含腳本在乾淨 terminal 重建，再從零核實。

## Notes — M12 (2026-06-20)

M12 grew the **first real content-aware strategy** onto the M11 dispatcher: log
compression. Instead of blind head/tail truncation, it classifies each line by
severity, drops the noise (`TRACE/DEBUG/INFO`) and keeps every signal line
(`WARN/ERROR/FATAL/CRITICAL`) plus anything unclassified, ending with a
`dropped N log lines | sha256:…` marker. The teaching point is the trade-off, not
the byte count: an ERROR buried in the **middle** of a noisy log survives here but
would be truncated away by head/tail. To avoid swallowing all-error logs, the
strategy only claims input when noise is ≥ 30% of the lines — otherwise it falls
through to `truncate`. Word matching is whole-word and byte-level ASCII
(`INFORMATION ≠ INFO`) so Python and Rust classify identically. The pre-existing
`01_messy_full` fixture turned out to be a real log and now routes through this
strategy (14473→1147, vs 2524 under truncate); `06_noisy_log` was added to lock
the keep-middle-errors branch in the parity gate.

## Notes — M12（2026-06-20）

M12 在 M11 dispatcher 上長出**第一片真正的內容感知策略**：log 壓縮。不再盲目頭尾
截斷，而是逐行依嚴重度分類，丟噪音（`TRACE/DEBUG/INFO`）、保所有訊號行
（`WARN/ERROR/FATAL/CRITICAL`）與其他行，末尾附 `dropped N log lines | sha256:…`
標記。教學重點是取捨而非位元數：埋在噪音**中段**的 ERROR 在這裡會存活，頭尾截斷則
會把它連同中段一起丟掉。為免吃掉全是 error 的 log，策略只在噪音佔比 ≥ 30% 時才認領，
否則落到 `truncate` 兜底。整詞、byte 級 ASCII 邊界比對（`INFORMATION ≠ INFO`），讓
Python 與 Rust 分類逐字節一致。既有 `01_messy_full` fixture 原來是一份真 log，現在改
走此策略（14473→1147，truncate 時是 2524）；新增 `06_noisy_log` 把「保留中段
error」分支鎖進 parity gate。

## Notes — M13 (2026-06-22)

M13 added the **second content-aware strategy** to the dispatcher: diff compression.
A unified/git diff classifies cleanly by line role — unchanged context lines start
with a space and are pure noise; everything else is structure worth keeping. So the
strategy drops the ` `-prefixed context and keeps every hunk header (`@@`), file
header (`diff`/`index`/`---`/`+++`), and `+`/`-` change line, ending with a
`dropped N diff context lines | sha256:…` marker. The win mirrors log: a change
buried in a large context block survives here, but head/tail truncation would lose
it — and the hunk header still carries the exact line ranges, with the CCR store
holding the full original for reversal. Sniffing requires an actual `@@` hunk header
(so markdown `+`/`-` bullet lists aren't mistaken for a diff) and ≥ 30% droppable
context (else it falls through to `truncate`). It registers **before** log: a
`@@`-bearing diff's structure should outrank log severity classification, and real
logs carry no hunk header so the two never collide. As with log, the design sticks to
pure ASCII byte prefixes (`starts_with(" ")` / `"@@"`) instead of `strip`/`trim` to
dodge the Python-unicode-whitespace vs Rust divergence. New `07_diff.json` fixture
runs the full pipeline 7227→1857 bytes, byte-for-byte across both languages.

## Notes — M13（2026-06-22）

M13 在 dispatcher 上接了**第二片內容感知策略**：diff 壓縮。unified/git diff 可依
「行角色」乾淨分類——未變更的 context 行以空格開頭、純屬噪音，其餘都是該留的結構。
策略丟掉 ` ` 開頭的 context，保留每個 hunk header（`@@`）、檔頭
（`diff`/`index`/`---`/`+++`）與所有 `+`/`-` 變更行，末尾附
`dropped N diff context lines | sha256:…` 標記。好處與 log 同源：埋在大段 context 中
的變更在這裡會存活，頭尾截斷則會丟掉它——而 hunk header 仍帶著精確行號範圍，CCR
store 也保有完整原文可逆。嗅探必須有真正的 `@@` hunk header（避免把 markdown 的
`+`/`-` 條列誤判成 diff）且可丟 context ≥ 30%（否則落到 `truncate` 兜底）。它註冊在
log **之前**：帶 `@@` 的 diff 結構應優先於 log 嚴重度分類，而真 log 不帶 hunk header
故兩者不衝突。與 log 一致，刻意全用 ASCII byte 前綴（`starts_with(" ")` / `"@@"`）而非
`strip`/`trim`，避開 Python unicode 空白與 Rust 分岔的地雷。新增 `07_diff.json`
fixture 走完整 pipeline 壓 7227→1857 bytes，兩語言逐字節一致。

## Notes — M14 (2026-06-22)

M14 added the **third content-aware strategy**: search compression for grep/rg
`file:lineno:content` output. The noise is many repeated matches in the same file,
so the strategy caps each file to its first 3 matches, drops the rest, and appends a
`dropped N search result lines | sha256:…` marker. The interesting part is a parity
landmine: the obvious match-line detector — "split on `:`, second field is all
digits" — also matches a log timestamp like `10:30:45`, which would make search
cannibalize logs and undo M12. The fix is a single robust discriminator: require the
file_key (text before the first `:`) to contain a `/`. Real `grep -rn pattern .`
output always carries a path (`./src/foo.py:12:`); a timestamp prefix
(`2026-06-20T10`) never does. It's a pure ASCII byte check (`'/' in key`), identical
across Python and Rust. With that, search registers as `(DIFF, SEARCH, LOG,
TRUNCATE)` — a grep over log files routes to search, but the existing noisy-log
fixtures carry no `/`-bearing `file:line:` lines so they still route to log, with no
regression. Determinism comes from an in-order scan plus per-file counts in a map
that is only ever looked up, never iterated, so there's no hash-ordering dependence.
New `08_search.json` runs the full pipeline 4289→1702 bytes, byte-for-byte.

## Notes — M14（2026-06-22）

M14 接上**第三片內容感知策略**：search 壓縮，對象是 grep/rg 的
`file:lineno:content` 輸出。噪音是同一檔案的大量重複命中，所以策略把每檔上限壓到
前 3 筆、丟掉其餘，末尾附 `dropped N search result lines | sha256:…` 標記。有意思的
是一個 parity 地雷：最直覺的 match line 判斷——「以 `:` 分隔、第二欄全是數字」——
會同時命中 log 時間戳 `10:30:45`（它字面上就是 `欄:數字:欄`），這會讓 search 反過來
吃掉 log、推翻 M12。解法是一個夠穩的判別式：要求 file_key（首個 `:` 之前）含有 `/`。
真實 `grep -rn pat .` 輸出一定帶路徑（`./src/foo.py:12:`），時間戳前綴
（`2026-06-20T10`）則不會——這是純 ASCII byte 檢查（`'/' in key`），Python 與 Rust
一致。據此 search 註冊為 `(DIFF, SEARCH, LOG, TRUNCATE)`：grep 搜 log 檔會走 search，
但既有的噪音 log fixture 沒有帶 `/` 的 `file:line:` 行，仍走 log、不回歸。確定性來自
保序逐行掃 + per-file 計數的 map——只查找、從不迭代，故無雜湊順序依賴。新增
`08_search.json` 走完整 pipeline 壓 4289→1702 bytes，逐字節一致。

## Notes — M15 (2026-06-22)

M15 added the **fourth content-aware strategy**: json compression. It finds the
largest JSON array, keeps its first 5 and last 2 elements, splices a marker string
element in between (so the result is still valid JSON), and copies everything outside
that array verbatim. The headline is a parity lesson. The obvious implementation —
parse the JSON, truncate the array, re-serialize — walks straight into the number
landmine flagged earlier: Python's `json.dumps` normalizes `1.10` to `1.1`, while
Rust's `serde_json` with `arbitrary_precision` keeps `1.10`, so the two byte streams
diverge. The fix is to **never re-serialize a value**. A byte-level structural scan
(tracking string literals so brackets/commas inside strings don't count, and nesting
depth so commas only split the innermost array) records each element's byte span;
kept elements are copied as raw slices. The only newly-written bytes are the
structural `[`, `,`, `]` and the marker — all ASCII constants — so Python and Rust
cannot disagree on a value. Tie-breaking is explicit (most elements, then smallest
start offset) because Python `max` keeps the first equal element while Rust
`max_by_key` keeps the last. The strategy only claims genuine JSON documents (first
non-whitespace byte is `[` or `{`), so prose or logs that merely contain brackets are
left alone, and it registers first. The new `09_json.json` fixture deliberately mixes
`1.10` and `1e10` into the array: it compresses 2959→1651 bytes, byte-for-byte across
both languages, with those numbers preserved verbatim — the proof that the
copy-don't-reserialize approach defuses the landmine.

## Notes — M15（2026-06-22）

M15 接上**第四片內容感知策略**：json 壓縮。它找出元素最多的 JSON array，保留前 5 +
後 2 個元素、中間塞一個 marker 字串元素（結果仍是合法 JSON），array 以外的 bytes
全部照抄。重點是一個 parity 教訓。最直覺的做法——parse、截斷、重新序列化——會正面
撞上先前點出的數字地雷：Python 的 `json.dumps` 把 `1.10` 正規化成 `1.1`，而 Rust 的
`serde_json`（開 `arbitrary_precision`）保留 `1.10`，兩邊 byte 流就分岔了。解法是
**絕不重序列化任何值**：用 byte-level 結構掃描（追蹤字串字面值，讓字串內的括號/逗號
不算數；追蹤巢狀深度，讓逗號只切最內層 array）記錄每個元素的 byte span，被保留的元素
照抄原始切片。唯一新寫入的 bytes 是結構字元 `[` `,` `]` 與 marker——全是 ASCII 常數——
所以 Python 與 Rust 不可能在值上分岔。tie-break 顯式寫死（元素最多，同票取 start 最小），
因為 Python `max` 同票保第一個、Rust `max_by_key` 保最後一個。策略只認領真正的 JSON
文件（首個非空白 byte 是 `[`/`{`），含括號的 prose/log 不碰；且註冊在最前。新增的
`09_json.json` fixture 刻意把 `1.10` 與 `1e10` 混進 array：壓 2959→1651 bytes、兩語言
逐字節一致、那些數字原樣保留——正是「照抄而不重序列化」拆掉地雷的證明。

## Notes — M16 (2026-06-27)

M16 added the **fifth content-aware strategy**: stack trace compression. It segments
a trace into frames — a frame header is a line that, after stripping leading ASCII
whitespace, begins with `File "` (Python) or `at ` + a `(` (Java/JS); the `(`
requirement rejects prose like "at the store" — and a frame includes the header plus
its indented continuation lines. The strategy keeps the first 3 and last 3 frames,
drops the middle frames, and keeps **every non-frame line**: the `Traceback` header,
the trailing `XxxError: message`, and any chained-exception separators are all signal.
The marker (`dropped N stack frames` + `content_key`) is emitted once at the first
dropped line; the rest are simply omitted. The win over blind truncation is frame
integrity: truncate cuts at a line boundary and can split a `File "..."` header from
its code line, and when a trace ends with several non-frame message lines the tail
window can push the crucial error line out — the frame-aware strategy never splits a
frame and the error message always survives. It registers `(JSON, DIFF, SEARCH, LOG,
STACKTRACE, TRUNCATE)`, just before the catch-all, so every existing fixture is
claimed by an earlier strategy first — zero regression by construction (a pure stack
trace carries no `INFO/DEBUG` noise, so log never claims it). Determinism comes from
in-order frame segmentation and pure index math (no hash-order dependence), and all
classification is byte-level (no `strip`/`trim` unicode divergence), so Python and
Rust stay byte-identical. The new `10_stacktrace.json` fixture — a 30-frame Python
recursion trace — compresses 2906→1386 bytes through the full pipeline, byte-for-byte
across both languages.

## Notes — M16（2026-06-27）

M16 接上**第五片內容感知策略**：stack trace 壓縮。它把 trace 切成 frame——frame 標頭
是「去掉前綴 ASCII 空白後，以 `File "`（Python）或 `at ` + 含 `(`（Java/JS）起頭」的行，
那個 `(` 的要求擋掉 "at the store" 這類 prose——一個 frame 含標頭行加其縮排續行。策略
保留前 3 + 後 3 個 frame、丟中段 frame，並保留**所有非 frame 行**：`Traceback` 標頭、
結尾的 `XxxError: 訊息`、以及 chained-exception 分隔線都是訊號。marker（`dropped N
stack frames` + `content_key`）只在第一個丟棄處塞一次，其餘丟棄行直接省略。勝過盲截斷
的點是 frame 完整性：truncate 在行邊界切，可能把 `File "..."` 標頭與其程式碼續行拆開；
而 trace 尾端有多行非 frame 訊息時，tail 視窗可能把最關鍵的錯誤行擠出去——frame 感知
策略永不切半個 frame、錯誤訊息恆保留。註冊 `(JSON, DIFF, SEARCH, LOG, STACKTRACE,
TRUNCATE)`、排在 catch-all 前，所以每個既有 fixture 都被前面的策略先接走——靠註冊順序
保證零回歸（純 stack trace 無 `INFO/DEBUG` 噪音，log 不會認領）。確定性來自保序 frame
切段 + 純 index 數學（無雜湊順序依賴），所有判別皆 byte 級（無 `strip`/`trim` unicode
分岔），故 Python 與 Rust 逐字節一致。新增的 `10_stacktrace.json` fixture——30 個 frame
的 Python 遞迴 trace——走完整 pipeline 壓 2906→1386 bytes、兩語言逐字節一致。

## Notes — M17 (2026-06-29)

M17 added the **sixth content-aware strategy**: CSV/table compression. It keeps the
header row plus the first 3 and last 2 data rows, and collapses the middle
homogeneous rows into a single marker (`dropped N table rows` + `content_key`). The
win over blind truncation is semantic: truncate doesn't understand that the header is
the signal — once there are enough data rows to push the column names out of the
`HEAD_LINES` window, the names are gone and the surviving number rows can't be read;
the CSV strategy pins the header to output line 0 and pairs it with representative
head/tail rows. Detection is deliberately conservative: after dropping a single
trailing newline, **every** non-empty line must contain the same delimiter (`,`
first, then `\t`) the same number of times (≥1) — prose can't keep an identical comma
count on every line, so this strong-signal check rejects false positives; an interior
blank line, or a quoted comma that breaks the equal-count invariant, simply falls
through to truncate (conservative, acceptable). Delimiter counting is pure ASCII byte
counting (`,` and `\t` are never UTF-8 continuation bytes), byte-identical to Python's
`str.count`; the Rust side guards `data_rows < HEAD+TAIL+MIN_DROP` first to avoid a
usize underflow. It registers `(JSON, DIFF, SEARCH, LOG, STACKTRACE, CSV, TRUNCATE)`,
just before the catch-all, so every existing fixture is claimed by an earlier strategy
first — zero regression by construction (all 10 prior fixtures keep their exact byte
counts). The new `11_csv.json` fixture — a 60-row table — compresses 3533→1095 bytes
through the full pipeline, byte-for-byte across both languages.

## Notes — M17（2026-06-29）

M17 接上**第六片內容感知策略**：CSV/表格 壓縮。它保留表頭列 + 前 3 + 後 2 筆資料列，
把中段同構資料列收斂成單一 marker（`dropped N table rows` + `content_key`）。勝過盲
截斷的點是語意：truncate 不懂「表頭才是訊號」——資料列一多就把欄名擠出 `HEAD_LINES`
視窗，欄名遺失後剩下的數字列全無法判讀；CSV 策略把表頭釘在輸出第 0 行、再配頭尾代表
列。嗅探刻意保守：去掉單一尾端換行後，**每一**非空行都必須以同一 delimiter（`,` 優先、
再 `\t`）出現相同次數且 ≥1——散文不可能每行逗號數都一致，這個強訊號檢查藉此擋掉誤判；
內部空行、或破壞「每行同數」的引號內逗號，一律落 truncate 兜底（保守、可接受）。
delimiter 計數是純 ASCII byte 計數（`,` 與 `\t` 皆非 UTF-8 續位元組），與 Python
`str.count` 逐字節一致；Rust 側先擋 `data_rows < HEAD+TAIL+MIN_DROP` 以避 usize 下溢。
註冊 `(JSON, DIFF, SEARCH, LOG, STACKTRACE, CSV, TRUNCATE)`、排在 catch-all 前，所以每個
既有 fixture 都被前面策略先接走——靠註冊順序保證零回歸（既有 10 fixture 位元數全不變）。
新增的 `11_csv.json` fixture——60 列表格——走完整 pipeline 壓 3533→1095 bytes、兩語言逐字節一致。

## Run / 執行

```bash
# Python (uv-managed 3.13 venv; fastapi doesn't support 3.14 yet)
cd rewrite && uv run pytest -q              # 118 tests

# Rust (standalone workspace)
cd rewrite/rust-lite && cargo test          # 121 tests

# Cross-language parity gate / 跨語言 parity gate（11 fixtures, byte-for-byte）
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
