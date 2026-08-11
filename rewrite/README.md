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
| M18 | `10e3522b` | seventh content-aware strategy: **Markdown table compression** | keep the header row + the **separator row** (`\|---\|---\|`) + first 3 + last 2 data rows, collapse the middle homogeneous rows into a single marker (`dropped N markdown table rows` + `content_key`). Distinct from CSV (M17), not just a different delimiter: a markdown table has an extra **separator row** that must be pinned — it defines column alignment and is a required part of a valid GFM table, and blind truncation would push both the header and separator out of the window. Detection is conservative/strong-signal: after dropping a single trailing newline, **every** line must contain the same number (≥1) of `\|`, **and** the second line must be a valid separator (only `\|`, `:`, `-`, space, with at least one `-`). The separator row is the key discriminator from CSV/prose — CSV data has no pipes, prose has neither an identical pipe count nor a separator row. Pipe/separator counting is pure ASCII byte counting (`\|`=0x7C/`-`=0x2D/`:`=0x3A are never UTF-8 continuation bytes) ⇒ identical to Python `str.count`. Registered `(JSON, DIFF, SEARCH, LOG, STACKTRACE, MARKDOWN, CSV, TRUNCATE)` — markdown before csv (more specific; the two are mutually exclusive, pipe vs comma), so a genuine markdown table is never grabbed by csv; the existing csv fixture has no pipes so markdown doesn't claim it (zero regression: all 10 prior fixtures keep their exact byte counts). New `12_markdown.json` fixture (40-row table): 3121→**1262** bytes through the full pipeline, cross-language byte-for-byte |
| M19 | `7fe44e8b` | eighth content-aware strategy (first **character-range**): **base64/hex blob compression** | the first intra-line strategy — the prior seven all drop whole lines or array elements; this one slices head/tail *within a line* by character offset. Finds the longest contiguous run of blob characters (base64/base64url/hex set, **no newlines or spaces** ⇒ a single token, targeting single-line data URIs), keeps the first 64 + last 64 characters, splices a marker (`dropped N blob chars` + `content_key`) into the middle, copies the rest of the text verbatim. **Parity key:** character-range slicing diverges between Python (code points) and Rust (bytes), so the strategy **requires the whole text to be ASCII** (`isascii`/`is_ascii`) — under ASCII, code point == byte, so both languages slice at identical offsets; non-ASCII is never claimed (falls through to truncate), and blobs are ASCII anyway. Detection is conservative/strong-signal: the contiguous run must be ≥512 chars — prose can't hold 512 chars with no spaces, and minified code/URLs are broken by `.;,?&{}()`. Limitation (honest): single-line blobs only (the run doesn't cross newlines); MIME/PEM line-wrapped base64 is a future extension. Registered `(JSON, DIFF, SEARCH, LOG, STACKTRACE, MARKDOWN, CSV, BLOB, TRUNCATE)` — last before the catch-all (very specific), so a single-line blob (no newlines) is claimed by no multi-line strategy and blob catches what would otherwise fall to truncate yet can't be compressed as one line (zero regression: all 12 prior fixtures keep their exact byte counts). New `13_blob.json` fixture (2600-char data URI): 2950→**948** bytes through the full pipeline, cross-language byte-for-byte |
| M20 | `11103a5b` | ninth content-aware strategy: **HTML/XML compression** | keep the markup structure and visible text, replace the inner content of `<script>`/`<style>` elements and `<!-- -->` comments with a marker (`dropped N html noise chars` + `content_key`). Beats blind truncation on semantics: truncate doesn't know a giant inline JS bundle is noise while structure and text are signal — once the scripts are big enough they push the real page content out of the window; HTML surgically removes only script/style/comment inner content. **Parity key (reuses the M15 JSON approach, non-ASCII safe):** Python finds/slices by **character index**, Rust by **byte index** — each indexes natively into the same logical positions (the same `<script>`, the same `>`), so the extracted logical substrings match and the output bytes are identical. This means non-ASCII text content (e.g. a Chinese page) is preserved verbatim and byte-for-byte across both languages (`14_html.json` contains Chinese). Tag names are matched lowercase only (conservative; dodges the unicode-`lower()` length-shift index trap), and every cut lands on an ASCII tag boundary so UTF-8 is never split. Registered `(..., CSV, HTML, BLOB, TRUNCATE)` — HTML before blob (a page with inline script routes to HTML and keeps its structure rather than being swallowed by blob as one giant run; a data URI has no `<script>` so it falls to blob). Existing fixtures have no noise regions ⇒ zero regression (all 13 prior fixtures keep their exact byte counts). New `14_html.json` fixture (scraped page with inline script+style+comment, Chinese body): 5760→**1244** bytes through the full pipeline, cross-language byte-for-byte |

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

## Notes — M18 (2026-06-29)

M18 added the **seventh content-aware strategy**: Markdown table compression. It keeps
the header row, the **separator row** (`|---|---|`), and the first 3 + last 2 data rows,
collapsing the middle homogeneous rows into a single marker (`dropped N markdown table
rows` + `content_key`). This is distinct from CSV (M17), not merely a different
delimiter: a markdown table carries an extra separator row that must be pinned — it
defines column alignment and is required for a valid GFM table, and blind truncation
would push both the header and the separator out of the window. Detection is
deliberately conservative/strong-signal: after dropping a single trailing newline,
**every** line must contain the same number (≥1) of `|`, **and** the second line must be
a valid separator (composed only of `|`, `:`, `-`, space, with at least one `-`). The
separator row is the key discriminator from CSV and prose — CSV data has no pipes, and
prose has neither an identical pipe count on every line nor a separator row. Pipe and
separator counting is pure ASCII byte counting (`|`/`-`/`:` are never UTF-8 continuation
bytes), byte-identical to Python's `str.count`; the Rust side guards `data_rows <
HEAD+TAIL+MIN_DROP` first to avoid a usize underflow. It registers `(JSON, DIFF, SEARCH,
LOG, STACKTRACE, MARKDOWN, CSV, TRUNCATE)` — markdown before csv (the more specific
signal; the two are mutually exclusive, pipe vs comma), so a genuine markdown table is
never grabbed by csv, and the existing csv fixture has no pipes so markdown leaves it
alone — zero regression (all 10 prior fixtures keep their exact byte counts). The new
`12_markdown.json` fixture — a 40-row table — compresses 3121→1262 bytes through the
full pipeline, byte-for-byte across both languages.

## Notes — M18（2026-06-29）

M18 接上**第七片內容感知策略**：Markdown table 壓縮。它保留表頭列、**分隔列**
（`|---|---|`）+ 前 3 + 後 2 筆資料列，把中段同構資料列收斂成單一 marker
（`dropped N markdown table rows` + `content_key`）。與 CSV（M17）的差別不只是換
delimiter：markdown 表格多一條分隔列必須釘住保留——它定義欄位對齊、是合法 GFM 表格
的必要結構，盲截斷會把表頭與分隔列一起擠出視窗。嗅探刻意保守、強訊號：去掉單一尾端
換行後，**每一行**都必須含相同數量（≥1）的 `|`，**且**第二行是合法分隔列（只由 `|`、
`:`、`-`、空白組成且至少一個 `-`）。分隔列是與 CSV/散文的關鍵鑑別子——CSV 資料無 pipe，
散文既不會每行 pipe 數一致、也沒有分隔列。pipe 與分隔列計數是純 ASCII byte 計數
（`|`/`-`/`:` 皆非 UTF-8 續位元組），與 Python `str.count` 逐字節一致；Rust 側先擋
`data_rows < HEAD+TAIL+MIN_DROP` 以避 usize 下溢。註冊 `(JSON, DIFF, SEARCH, LOG,
STACKTRACE, MARKDOWN, CSV, TRUNCATE)`——markdown 排 csv 前（更專一的訊號；兩者 pipe vs
逗號互斥），所以真 markdown 表格不會被 csv 誤搶，既有 csv fixture 無 pipe → markdown
不認領——零回歸（既有 10 fixture 位元數全不變）。新增的 `12_markdown.json` fixture——
40 列表格——走完整 pipeline 壓 3121→1262 bytes、兩語言逐字節一致。

## Notes — M19 (2026-06-29)

M19 added the **eighth content-aware strategy** and the **first character-range** one:
base64/hex blob compression. Every prior strategy drops whole lines or array elements;
this one slices head and tail *within a single line* by character offset. It finds the
longest contiguous run of blob characters (the base64/base64url/hex set, with **no
newlines or spaces** — a single token, which targets single-line data URIs), keeps the
first 64 + last 64 characters, and splices a marker (`dropped N blob chars` +
`content_key`) into the middle, copying the rest of the text verbatim. The parity key:
character-range slicing diverges between Python (code points) and Rust (bytes), so the
strategy **requires the whole text to be ASCII** — under ASCII a code point equals a
byte, so both languages slice at identical offsets; non-ASCII content is never claimed
and falls through to truncate, and real blobs are ASCII anyway. Detection is
deliberately conservative: the contiguous run must be ≥512 characters — prose can't hold
512 characters with no spaces, and minified code or URLs are broken up by `.;,?&{}()`.
Honest limitation: single-line blobs only (the run doesn't cross newlines); MIME/PEM
line-wrapped base64 is a future extension. It registers `(JSON, DIFF, SEARCH, LOG,
STACKTRACE, MARKDOWN, CSV, BLOB, TRUNCATE)` — last before the catch-all, very specific,
so a single-line blob (no newlines) is claimed by none of the multi-line strategies and
blob catches what would otherwise fall to truncate but can't be compressed as a single
line — zero regression (all 12 prior fixtures keep their exact byte counts). The new
`13_blob.json` fixture — a 2600-char data URI — compresses 2950→948 bytes through the
full pipeline, byte-for-byte across both languages.

## Notes — M19（2026-06-29）

M19 接上**第八片內容感知策略**，也是**第一片「字元範圍」**策略：base64/hex blob 壓縮。
前七片全是丟整行或 array 元素；這片在**一行之內**按字元偏移切頭尾。它找最長的「連續 blob
字元串」（base64/base64url/hex 字元集、**不含換行/空白**＝單一 token，鎖定單行 data URI），
保前 64 + 後 64 字元，中段塞一個 marker（`dropped N blob chars` + `content_key`）、串外
bytes 照抄。parity 正解：字元範圍切片在 Python（依 code point）與 Rust（依 byte）天生分岔，
所以本策略**要求整段 text 為純 ASCII**——ASCII 下 code point 與 byte 一對一，兩語言切片偏移
完全一致；非 ASCII 一律不認領、落 truncate 兜底，而真實 blob 本就純 ASCII。嗅探刻意保守：
連續串須 ≥512 字元——散文不可能 512 字元不含空白，minified 程式碼/URL 也會被 `.;,?&{}()`
打斷。誠實的限制：只認單行 blob（run 不跨換行）；MIME/PEM 換行折疊的 base64 留作未來擴充。
註冊 `(JSON, DIFF, SEARCH, LOG, STACKTRACE, MARKDOWN, CSV, BLOB, TRUNCATE)`——排在 catch-all
前、極專一，單行 blob（無換行）不被任何多行策略認領，blob 接住「否則落 truncate 卻因單行
無法壓」的巨型 blob——零回歸（既有 12 fixture 位元數全不變）。新增的 `13_blob.json`
fixture——2600 字元 data URI——走完整 pipeline 壓 2950→948 bytes、兩語言逐字節一致。

## Notes — M20 (2026-06-29)

M20 added the **ninth content-aware strategy**: HTML/XML compression. It keeps the
markup structure and the visible text, and replaces the inner content of `<script>` and
`<style>` elements (and `<!-- -->` comments) with a marker (`dropped N html noise chars`
+ `content_key`). The win over blind truncation is semantic: truncate doesn't know that
a giant inline JS bundle is noise while the structure and text are signal — once the
scripts are large enough they push the real page content out of the window; the HTML
strategy surgically removes only script/style/comment inner content and keeps the
boundaries and text. The parity key reuses the M15 JSON approach and is non-ASCII safe:
Python finds and slices by **character index** while Rust uses **byte index** — each
indexes natively into the same logical positions (the same `<script>`, the same `>`), so
the extracted logical substrings are identical and the output bytes match. Non-ASCII
text content (a Chinese page, say) is therefore preserved verbatim and byte-for-byte
across both languages (`14_html.json` carries a Chinese body). Tag names are matched
lowercase only — conservative, and it dodges the unicode-`lower()` length-shift index
trap — and every cut lands on an ASCII tag boundary so UTF-8 is never split. It registers
`(..., CSV, HTML, BLOB, TRUNCATE)`: HTML before blob, so a page with an inline script
routes to HTML and keeps its structure instead of being swallowed by blob as one giant
run, while a data URI (no `<script>`) falls through to blob. Existing fixtures have no
noise regions, so there is zero regression (all 13 prior fixtures keep their exact byte
counts). The new `14_html.json` fixture — a scraped page with inline script, style, and
a comment, plus a Chinese body — compresses 5760→1244 bytes through the full pipeline,
byte-for-byte across both languages.

## Notes — M20（2026-06-29）

M20 接上**第九片內容感知策略**：HTML/XML 壓縮。它保留標籤結構與可見文字，把 `<script>`、
`<style>` 元素的內文與 `<!-- -->` 註解換成一個 marker（`dropped N html noise chars` +
`content_key`）。勝過盲截斷的點是語意：truncate 不懂「巨型 inline JS 是噪音、結構與文字才是
訊號」——script 一大就把頁面真正內容擠出視窗；HTML 策略精準只挖 script/style/comment 內文、
保住邊界與文字。parity 正解沿用 M15 JSON 模式、非 ASCII 安全：Python 用 **char index**
find/slice、Rust 用 **byte index**，各自原生索引定位同一邏輯位置（同一個 `<script>`、同一個
`>`）→ 切出的邏輯子字串相同、輸出 bytes 一致。所以中文網頁等非 ASCII 文字內容逐字保留、兩
語言逐字節一致（`14_html.json` 帶中文 body）。標籤名只比對小寫（保守，且避開 unicode
`lower()` 改變長度的 index 陷阱），切點都落在 ASCII 標籤邊界 → 不破 UTF-8。註冊
`(..., CSV, HTML, BLOB, TRUNCATE)`：HTML 排 blob 前，含 inline script 的頁面走 HTML 保結構、
不被 blob 當一條巨串吞掉；data URI（無 `<script>`）則落 blob。既有 fixture 皆無噪音區 → 零
回歸（既有 13 fixture 位元數全不變）。新增的 `14_html.json` fixture——帶 inline
script+style+註解、且 body 為中文的爬取頁面——走完整 pipeline 壓 5760→1244 bytes、兩語言逐字節一致。

## Notes — M21 (2026-08-09)

M21 is the first milestone that came out of **reading the answer key rather than building
forward** (see `READING-02-log-compressor.md`), and it fixes a defect rather than adding a
strategy. The M12 log strategy classified lines by an ASCII-uppercase severity vocabulary
(`ERROR`/`WARN`/`INFO`…) — the genre of an *application runtime log*. Build and test runners
speak a different genre: pytest says `FAILED`, cargo says `error[E0382]`, jest uses glyphs.
None of them match, so `classified == 0`, `drop == 0`, `applies()` returned false, and the
input fell through to blind head/tail truncation. Measured on an 85-line pytest run: 85 → 31
lines with the entire `FAILURES` section removed — precisely the mid-log-error loss that
M12's own design note claims the strategy prevents. The fix deliberately does **not** extend
the token table, because that is an open set (finish pytest and cargo is next). It adds a
*structural* signal instead: a progress line is a long run of status glyphs, a shape that is
independent of any tool's wording. Two conditions must both hold — a run of ≥ 8 glyphs from
`.sxXFEP`, **and** the line either ends in `%]` or consists only of glyphs and whitespace.
The second condition is not optional: a table-of-contents dot leader (`Chapter 3 .......... 42`)
can be a dozen dots long, so run length alone misfires; the tests cover both sides of that
boundary. The change lands in `_severity()`, not `_log_applies()` — fixing `applies` alone
would make the strategy claim the input and then compress nothing, since `squeeze` returns
the text unchanged when it finds no droppable lines. One parity landmine surfaced during
implementation: Rust's `str::trim()` strips Unicode whitespace (including U+3000) while
Python's `bytes.strip()` strips only ASCII `b" \t\n\r\x0b\x0c"`, so the Rust side strips
bytes explicitly (note `\x0b` is absent from `is_ascii_whitespace()`) and a test locks it.
The new tests add a class of assertion the suite never had — **"the input it should claim,
it does claim"**; every prior log test fed input already guaranteed to carry tokens and then
checked behaviour, which is exactly how this defect stayed green for two months. New fixture
`15_pytest.json` covers the new path (the 14 existing fixtures passing only proved the old
paths were intact): 5831 → 1980 bytes, 77 lines → 27, 50 progress lines dropped, both
`AssertionError`s, both file:line references and the summary all preserved, byte-for-byte
across both languages. Gates: Python 154 → 162, Rust 154 → 158, parity 14 → 15 all pass,
clippy 0. Every pre-existing fixture keeps its exact byte count.

**Known limitation:** this covers pytest-style progress output. cargo and jest emit neither
severity tokens nor glyph runs, so they still fall through to truncation. The real answer is
the industrial version's step 1 — format detection per genre — not more heuristics here.

## Notes — M21（2026-08-09）

M21 是第一個**從讀解答本而非往前建**得出的里程碑（見 `READING-02-log-compressor.md`），
而且它修的是缺陷、不是加策略。M12 的 log 策略用 ASCII 大寫 severity 詞彙表
（`ERROR`/`WARN`/`INFO`…）分類，那是「應用程式 runtime log」的體裁；建置與測試工具講的是
另一種話：pytest 說 `FAILED`、cargo 說 `error[E0382]`、jest 用符號 —— 一個都不命中，於是
`classified == 0`、`drop == 0`、`applies()` 回 false，輸入落到盲目頭尾截斷。實測 85 行
pytest 輸出：85 → 31 行，`FAILURES` 整段消失 —— 正是 M12 自己的設計註解宣稱這支策略要
避免的「中段 error 被丟掉」。修法刻意**不**擴充 token 表，因為那是開放集合（補完 pytest
還有 cargo、jest、make，而「另一種工具碰巧不長這樣」永遠有下一個）。改用**結構**訊號：
進度行是一長串狀態符號，這個形狀與工具的用詞無關。兩個條件須同時成立 —— 存在長度 ≥ 8 的
`.sxXFEP` 連續段，**且**該行以 `%]` 收尾或整行只有進度符號與空白。第二條不可省：目錄的
點狀填充（`Chapter 3 .......... 42`）可以有十幾個點，只看連續長度會誤判，測試涵蓋這條
邊界的兩側。修補點在 `_severity()` 而非 `_log_applies()` —— 只改 applies 會讓策略認領卻
壓不掉任何東西（squeeze 找不到可丟行就原文回），已實測確認。實作中抓到一個 parity 地雷：
Rust 的 `str::trim()` 剝 Unicode 空白（含 U+3000），Python 的 `bytes.strip()` 只剝 ASCII
`b" \t\n\r\x0b\x0c"`，同一行輸入兩語言會分岔；Rust 端改為自行逐字節剝（注意 `\x0b` 不在
`is_ascii_whitespace()` 裡），並加測試鎖住。新測試補的是這套測試從來沒有的一類斷言 ——
**「它該認領的輸入，它有認領」**；既有 log 測試全都先餵保證含 token 的輸入再驗行為正確，
這正是本缺陷能綠燈潛伏兩個月的原因。新增 fixture `15_pytest.json` 覆蓋新路徑（既有 14 個
fixture 全過只證明舊路徑沒壞）：5831 → 1980 bytes、77 行壓成 27 行、丟掉 50 行進度行，
兩處 `AssertionError`、兩處檔名行號與 summary 全部存活，兩語言逐字節一致。閘門：
Python 154 → 162、Rust 154 → 158、parity 14 → 15 全過、clippy 0。既有 fixture 位元數全不變。

**已知限制**：本次只覆蓋 pytest 形狀的進度輸出。cargo 與 jest 既無 severity token 也無
符號連續段，仍會落到截斷。真正的解是工業版 pipeline 的第 1 步 —— 依體裁做格式偵測 ——
而不是在這裡繼續堆啟發式。

## Notes — M22 (2026-08-09)

M22 is the rebuild's **first strategy that selects by information rather than by position**,
and like M21 it came out of reading the answer key (`READING-03-smart-crusher.md`). Every
strategy up to this point asked "where does this line/record sit in the input"; the M15 JSON
strategy keeps `JSON_HEAD = 5` leading and `JSON_TAIL = 2` trailing elements of the largest
array. Measured on 100 API health-check records — 97 `ok`, 3 `timeout` buried at indices
48–50 — that compressed 6828 → 550 bytes (92% saved) and **dropped all three timeouts**,
leaving seven identical `status: ok` rows from which a model would conclude the system is
healthy. This is more dangerous than the M21 defect precisely because the output looks
entirely reasonable and the compression ratio looks like a win: the metric is green and the
conclusion is wrong. The criterion is lifted from `smart_crusher`'s `detect_rare_status_values`
— specifically the version **after its bug #3 fix**, because the original guard
`if not (2 <= len(unique_values) <= 10): continue` switched rare-error preservation off
exactly when error-code cardinality was high, i.e. when it mattered most. The Pareto form:
distinct values in [2, 50]; sort value frequencies descending; find the smallest K whose
top-K covers ≥ 80% of items; if K ≤ 5, the remaining values are rare and the elements
carrying them join the must-keep set. Three distributions are tested, including the one that
must **not** fire: low-cardinality-with-a-dominant-value fires, bimodal (60 `info` + 25 `warn`
+ 15 distinct rare errors) fires — the case the pre-fix algorithm missed entirely — and a
uniform distribution never reaches 80% with K ≤ 5 and is correctly identified as
non-categorical. The keep cap is derived from the same criterion: Pareto already bounds a
single field's rare set at 20%, but the union across several categorical fields can exceed
that, so the union gets the same 20% bound. An absolute cap of 10 was written first and was
wrong — with exactly 15 rare values it silently discarded 5 of the most informative elements,
the very thing this strategy exists to prevent; the bimodal test caught it. Output format:
one marker per contiguous dropped run, so an input with no rare values produces a single run
and is byte-identical to pre-M22 (all 15 existing fixtures keep their exact byte counts,
`09_json` still 2959 → 1651). Parity: the 80% threshold is integer arithmetic
(`cum * 100 >= total * 80`) to avoid float divergence, and `BTreeMap` plus explicit sorting
(frequency descending, value ascending on ties) keeps iteration order identical across
languages; key/value pair extraction reuses the existing bracket-and-string scan rather than
parsing JSON. New fixture `16_rare_records.json`: 9275 → 1987 bytes, keeping ids
`[0,1,2,3,4, 48,49,50, 98,99]`. Compression drops from 92% to 79% and the answer goes from
wrong to right — the trade this milestone deliberately makes. Gates: Python 162 → 169,
Rust 158 → 164, parity 15 → 16 all pass, clippy 0.

## Notes — M22（2026-08-09）

M22 是重建**第一個按資訊而非按位置選擇**的策略，和 M21 一樣出自讀解答本
（`READING-03-smart-crusher.md`）。在此之前每一支策略問的都是「這一行/這一筆排在
第幾個」；M15 的 JSON 策略保留最大 array 的頭 `JSON_HEAD = 5`、尾 `JSON_TAIL = 2`。
實測 100 筆 API 健檢結果 —— 97 筆 `ok`、3 筆 `timeout` 埋在第 48–50 —— 壓
6828 → 550 bytes（省 92%）而**三筆 timeout 全滅**，留下七筆一模一樣的 `status: ok`，
模型會據此判定系統一切正常。這比 M21 的缺陷更危險，正因為輸出看起來完全合理、
壓縮率還很漂亮：**指標亮綠燈，結論是錯的**。判準取自 `smart_crusher` 的
`detect_rare_status_values`，而且刻意用它**修好 bug #3 之後**的版本 —— 原本的
`if not (2 <= len(unique_values) <= 10): continue` 會讓「保留罕見錯誤」在錯誤種類
一多時自己關掉，也就是最需要它的時候。Pareto 形式：相異值數落在 [2, 50]；值頻率
降冪排序；找最小的 K 使 top-K 覆蓋 ≥ 80% 的項目；若 K ≤ 5，其餘的值即罕見，帶有
它們的元素進必留集合。三種分布都測，包含**不該觸發**的那一種：低基數加主宰值會
觸發；雙峰（60 `info` + 25 `warn` + 15 種罕見錯誤）會觸發 —— 這正是修補前的演算法
整個漏掉的情況；均勻分布則永遠無法在 K ≤ 5 內達到 80%，被正確判定為非類別欄。
保留上限與判準同源：Pareto 已保證單一欄位的罕見集合 ≤ 20%，但多個類別欄的聯集
可能超過，所以對聯集再套一次 20%。第一版寫的絕對值上限 10 是錯的 —— 罕見值剛好
15 個時它會安靜丟掉 5 個最有資訊量的元素，正是這支策略要防的事，由 bimodal 測試
抓出來。輸出格式：每段連續丟棄各插一個 marker，因此無罕見元素的輸入只有一段、
與 M22 前逐字相同（既有 15 個 fixture 位元數全不變，`09_json` 仍 2959 → 1651）。
parity：80% 門檻用整數運算（`cum * 100 >= total * 80`）避免浮點分岔，`BTreeMap`
加顯式排序（頻率降冪、同頻值升冪）讓兩語言迭代順序一致；鍵值對抽取沿用既有的
括號/字串掃描，不解析 JSON。新增 fixture `16_rare_records.json`：9275 → 1987 bytes，
保留 id `[0,1,2,3,4, 48,49,50, 98,99]`。壓縮率從 92% 降到 79%，而答案從錯的變成
對的 —— 這是本里程碑刻意付出的代價。閘門：Python 162 → 169、Rust 158 → 164、
parity 15 → 16 全過、clippy 0。

## Notes — M23 (2026-08-10)

M23 is the rebuild's **first observer**: it never changes a byte. Everything before it —
including M3 cache stabilization — was a *normalizer*. `READING-05-cache-stabilization.md`
found that the industrial `cache_stabilization/` splits Phase E into observers
(`volatile_detector`, `drift_detector`, which never mutate the body) and normalizers
(behind gates), and that **the rebuild had only the normalizing half**. The division of
labour is the point: normalization fixes what the proxy can fix, observation surfaces what
only the customer can fix. A per-request timestamp in the customer's system prompt is not
the proxy's to delete — but staying silent about it is a choice too, because from the
outside a miss caused by volatile customer content and a miss caused by the proxy look
identical. This scans for ISO-8601 timestamps, UUID v4s, and ID-named fields
(`request_id` / `trace_id` / `session_id` / `correlation_id`), and writes one stderr
observation line per finding. No regex — every pattern is explicit byte-position checks.
The UUID rule is the sharp one: it keys on the version nibble at position 14, so it is not
looking for UUIDs, it is looking for UUIDs *that will change* (a build hash is not v4; a
fixed identifier would not vary between requests anyway).

**Ported naively, it was wrong on real input.** The answer key's `volatile_detector` walks
every message; run over the existing fixtures, `01_messy_full` and `06_noisy_log` each
produced **10 findings that all hit the cap**, every one of them located at `messages[2]` —
the last message, i.e. the live zone. That content is never in the cached prefix (M3's
`_place_breakpoints` puts marker 2 on `messages[-2]`), it changes every turn, and changing
is harmless. Worse than noise: the cap is global and the walk order is system → messages →
tools, so one timestamp-laden `tool_result` fills all ten slots and **silently crowds out
real findings in `tools`**. So M23 skips the last message. This mirrors the industrial
`drift_detector`, which deliberately skips the live-zone tail for the same reason — first
separate the changes that are expected, or the alarm is just noise. Three tests pin it:
live-zone content is not reported, `messages[-2]` still is, and a body with a noisy live
zone plus one `correlation_id` in `tools` must report exactly the `tools` finding.

Three deliberate divergences from the answer key, each documented in-module rather than
left to look like omissions. (1) The entry point takes **bytes** and parses its own copy;
the industrial version takes `&Value` to avoid a second parse and gates non-mutation with
`debug_assert_eq!` plus byte-equality integration tests. Paying one extra parse per POST
buys an invariant the borrow checker enforces: the scanner never holds the caller's object.
(2) `sample` never echoes customer values — see the review round below. (3) No `ApiKind`
split: the rebuild only ever speaks Anthropic `/v1/messages`, so a second walker would be a
branch with no second caller.

Parity landmines of the "the two languages disagree about what a character is" family:
`str.isdigit()` returns `True` for Arabic-Indic digits while `u8::is_ascii_digit` does not,
so both sides compare against ASCII `0-9` explicitly; and `str.lower()` is Unicode-aware and
can change a string's length, so ID-key matching folds only `A-Z` to match Rust's
`to_ascii_lowercase`.

Because the scan changes nothing, the byte-for-byte parity gate cannot see it — passing
phase 1 only proves the old paths still work (the same lesson M21 learned when it added
`15_pytest.json`). `parity.sh` therefore gained a **second phase** comparing the findings
themselves across languages over every fixture, plus `17_volatile.json`, which exists
because most fixtures have no volatile content and an all-empty comparison prints a wall of
`PASS` while verifying nothing.

### The review round: both reviewers blocked, and they were right

`rust-reviewer` and `python-reviewer` were run before pushing. Both returned BLOCK. Every
disputed claim was reproduced independently before acting on it; all of them held.

**The parity claim in the paragraph above used to be wrong, and this README asserted it.**
The original text said Python's `parse_float=str, parse_int=str` lined up with Rust's
`arbitrary_precision` so that a `trace_id` of `1.10` produced the same sample in both. That
is true only for trailing zeros after a decimal point — the one example that had been
tested. A differential harness found `1E5` renders as `1e+5` on the Rust side and `-0`
renders as `0`; `json.dumps` also re-quotes numbers-that-are-strings, so any ID field whose
value was an object containing a number diverged too. This is the "a formula derived from a
special case takes the test down with it" shape: the test guarded the read path and nothing
guarded the write path, and the false generalization was then written into the docs.

**The `sample` policy was rebuilt around never echoing customer content.** The ID-field
needles are matched as *substrings*, so `session_identity_token` matches `session_id` — and
that field's value is frequently a credential in its own right. Most API keys are shorter
than the old 80-character cap, so they were not even truncated. Since the matching set is
open-ended, the only safe rule is to never emit the value: `id_field` samples are now a type
descriptor (`string[38]`, `number`, `object[2]`, `array[5]`, `bool`), and `uuid_v4` samples
are redacted to an 8-character prefix because v4-shaped API keys are common. `location`
already pinpoints the field, which is all a user needs in order to act. This one change also
retired the entire number-literal divergence family and made character-vs-byte truncation
moot, since the longest sample is now 19 characters.

**Contract violations.** `_scan_value` raised `RecursionError` at ~1000 levels of nesting,
violating the stated "never raises" contract — and `except (ValueError, ...)` could not have
caught it. Worse, serde_json rejects at 128 nested containers while Python's parser does not,
so 127–999 was a silent divergence band. Python now mirrors serde_json's limit with an
*iterative* depth check (using recursion to measure recursion depth is how you blow the stack
you were trying to protect), pinned on both sides by `depth_126` / `depth_127`. Three more
input classes needed the same alignment: `NaN`, non-UTF-8 and BOM-prefixed bodies, and lone
surrogate escapes — Python accepts all three, serde_json rejects all three *for the whole
document*, so the rejection granularity had to match. Paired surrogates must still work, and
are tested, because a guard that also blocks what should pass is not a guard. On the Rust
side, `eprintln!` panics when stderr write fails (`proxy 2>&1 | tee log` with a departed
reader is EPIPE), on the one path that had just been declared panic-free.

**The cap was crowding out real findings.** `MAX_FINDINGS` counted hits, not locations, so
three timestamp-bearing frozen messages filled all ten slots across only three distinct
locations and silently displaced the one `session_id` finding in `tools`. The cap now counts
distinct `(kind, location)` pairs with a `count` per finding, and `truncated` is an explicit
signal — exactly-ten and gave-up-at-ten must not look identical. A 1 MiB scan budget bounds
the work: this runs *before* forwarding, so scan time is latency.

**The gate that would have caught all of it did not exist.** Seventeen fixtures touched none
of the eleven divergence classes — "the two implementations guard each other" was a claim
that had never been tested. `parity.sh` gained a **third phase**: 15 adversarial fixtures,
each with a **golden** expectation rather than a mere cross-language comparison, because
about half of the correct answers here are "both sides empty" and comparing empty to empty
passes while verifying nothing. The gate additionally asserts the fixture count and greps
every output for the planted secret. Gates: Python 169 → 200, Rust 164 → 191, parity 17
fixtures × 2 phases + 15 adversarial cases, clippy 0. End-to-end: posting the fixture through
the running proxy emits four observation lines and forwards bytes unchanged.

### review 第二輪：三條 HIGH，都是「修了一半就收手」

再跑一次兩支 reviewer。rust-reviewer 仍 BLOCK，三條 HIGH 每一條我都獨立複驗成立。

**`location` 是 C1 沒關掉的另一半。** sample 政策擋住了「命中欄位的值」，但
`location` 是**客戶自己的 key 名串起來的路徑**，而祖先 key 完全不受 needle 約束 ——
`{"cust-4021@example.com":{"Bearer sk-live-XYZ":{"trace_id":"v"}}}` 會把整串印進觀測線。
而且沒有長度上限：200 KB 的 key 名就是 200 KB 的單行 stderr（實測 200031 字元）。
**我用來否決舊 sample 政策的理由，原封不動適用於這裡，但我沒有把它套過去。**
處置是設界而非消除（location 是這筆 finding 唯一可行動的內容）：單段 40 字元、
總長 200 字元、超過則中段省略而保頭保尾。

**`eprintln!` 那條修了等於沒修。** 我加固了自己新寫的 `emit_volatile_observations`，
還在 commit message 裡寫「就在剛宣告絕不 panic 的那條路徑上」—— 但同一個 `forward()`
函式往下 25 行的 `proxy.rs:191` 仍是 `eprintln!`，而**它每個請求都跑**（含 GET），
觸發面比我修的那條更廣。端到端行為一個 byte 都沒變，只是把 panic 往後挪了幾行。
這是「會跑不等於有作用」的教科書版本：我驗證了新函式，沒驗證那個宣稱的目標。
現在 `forward()` 與 SSE 路徑都改用 `writeln!`，並實測 stderr reader 退場後連送 7 次
請求 proxy 仍存活。

**「屬效能而非正確性」的判斷被推翻，但推翻它的那個數字後來被收回了。** 每個節點
無條件 `format!` 一條 location（即使整份 body 零 findings），改成可變的片段堆疊
（push/pop），location 只在真的產生 finding 時才具體化 —— 這個改動是對的，它消除了
成本對結構深度的相依。

但這裡有一件更該記下來的事：當時 reviewer 給的證據是「1 MiB 深結構 114 ms / 166 MB
vs 同位元組數淺結構 1.6 ms / 4.3 MB」，**我沒有自己重跑就把它當成事實寫進程式碼註解
與這份 README**。下一輪 reviewer 自己重測後收回了那個數字（4.55 ms vs 2.26 ms，
2 倍不是 70 倍）。改動本身站得住，理由是設計上的；那個數字不是。
**引用別人的量測前要自己重跑一次** —— 這與本專案記過的「不可重現的增益＝沒有增益」
是同一條，只是這次搞錯的是我引用的方向。

同輪一併修掉的還有：`truncated` 一號多用（「撞上限」與「body 太大沒掃」共用一個
旗標，於是 proxy 會對沒掃過的 body 印出「已達 10 個相異位置的上限」—— 修掉第一層
歧義又長出第二層）拆成兩個訊號；`detect_volatile_content` 這個公開 API 在收手工建的
深 `Value` 時會 stack overflow **abort**（比 panic 更糟，連攔都攔不到）→ 走訪內加深度
守門，並誠實記下界線：守門讓走訪有界，但救不了 serde_json 自己的遞迴 `Drop`
（5000 層時它自己就 abort）。

**adversarial gate 自己也被打穿了。** 祕密外洩那道守門只斷言「輸出不含 SECRET」——
把 fixture 裡的祕密換成同長度但不含 needle 的值，grep 的 needle 從此不在輸入裡、
永遠不可能命中，整條 gate 全綠。**這正是我自己在 gate 註解裡寫的那個形狀，卻在同一份
檔案裡犯了一次。** 現在先正向斷言「祕密確實在 fixture 輸入裡」再斷言它不在輸出裡，
另加「非空 golden 數」的斷言（15 個 golden 有 6 個本來就是空的）。
閘門：Python 200 → 204、Rust 191 → 195、parity 三相全過、clippy 0。

## Notes — M23（2026-08-10）

M23 是重建**第一個觀測器**：一個 byte 都不改。在它之前的所有東西 —— 包含 M3 的
cache 穩定化 —— 都是**正規化器**。`READING-05-cache-stabilization.md` 讀出工業版的
`cache_stabilization/` 把 Phase E 切成觀測（`volatile_detector`、`drift_detector`，
絕不動 body）與正規化（會動，但受閘門管）兩類，而**重建只有正規化那一半**。這個
分工才是重點：正規化修的是 proxy 修得動的，觀測揭露的是只有客戶自己能修的。客戶
system prompt 裡每次現算的時間戳不是 proxy 該刪的 —— 但沉默也是一種選擇，因為從
外面看，「因為客戶內容易變而 miss」和「因為 proxy 弄壞而 miss」長得一模一樣。本片
掃 ISO-8601 時間戳、UUID v4、以及 ID 名稱欄位（`request_id` / `trace_id` /
`session_id` / `correlation_id`），每筆發現印一行 stderr 觀測線。不用 regex —— 每個
pattern 都是明寫的位元組位置檢查。UUID 那條判準最漂亮：它認第 14 位的 version
nibble，所以**它不是在找 UUID，是在找「會變的」UUID**（build hash 不是 v4；固定
識別碼本來就不會在請求之間變）。

**照抄過來，在真實輸入上是錯的。** 解答本的 `volatile_detector` 走訪全部 messages；
拿既有 fixture 一跑，`01_messy_full` 與 `06_noisy_log` 各噴 **10 筆、全部撞上限**，
而且 location 一律是 `messages[2]` —— 最後一則訊息，也就是 live zone。那段內容從來
不在快取前綴裡（M3 的 `_place_breakpoints` 把標記 2 放在 `messages[-2]`），它每輪都
變，而且變了無害。比噪音更糟的是：上限是全域的、走訪順序是 system → messages →
tools，光一則塞滿時間戳的 `tool_result` 就能占滿十個名額，**把 `tools` 裡真正該報的
東西安靜擠掉**。所以 M23 跳過最後一則。這與工業版 `drift_detector` 刻意跳過
live-zone 尾端是同一個道理 —— 先分清哪些變化是預期的，否則警報等於雜訊。三條測試
釘住它：live zone 的內容不得回報、`messages[-2]` 仍要回報、以及「live zone 全是噪音
＋ `tools` 裡一個 `correlation_id`」必須剛好只回報 `tools` 那一筆。

三處刻意偏離解答本，都寫在模組註解裡，而不是留著看起來像漏做的。(1) 入口吃
**bytes**、自己 parse 一份副本；工業版收 `&Value` 以省一次 parse，非變性靠呼叫端的
`debug_assert_eq!` 與逐位元組整合測試守。每個 POST 多付一次 parse，換到的是借用
檢查器替你強制的不變量：掃描器手上根本沒有呼叫端的物件。(2) `sample` 永不回吐客戶
的值 —— 見下面的 review 回合。(3) 不做 `ApiKind` 分歧：重建全程只講 Anthropic
`/v1/messages`，第二個 walker 會是一個沒有第二個呼叫者的分支。

parity 地雷屬於「兩個語言對『什麼算一個字元』意見不同」這一家：`str.isdigit()` 對
阿拉伯-印度數字回 `True` 而 `u8::is_ascii_digit` 不會，所以兩邊都明寫比對 ASCII
`0-9`；`str.lower()` 是 Unicode 感知的、可能改變字串長度，所以 ID key 比對只折
`A-Z`，對齊 Rust 的 `to_ascii_lowercase`。

因為掃描什麼都不改，byte-for-byte 的 parity gate 根本看不見它 —— 相 1 全過只證明
舊路徑沒壞（M21 補 `15_pytest.json` 時學到的同一課）。所以 `parity.sh` 多了**第二
相**，對每個 fixture 比對兩語言的 findings 本身，另加 `17_volatile.json`：多數
fixture 本來就沒有 volatile 內容，全空的比對會印一整排 `PASS` 而什麼都沒驗到。

### review 回合：兩支 reviewer 都擋下，而且他們是對的

push 之前跑了 `rust-reviewer` 與 `python-reviewer`，兩支都回 BLOCK。每一條有爭議的
指控都先獨立複驗過才動手 —— 結果全部成立。

**上一段的 parity 宣稱原本是錯的，而這份 README 白紙黑字這樣寫過。** 原文說 Python 的
`parse_float=str, parse_int=str` 對齊了 Rust 的 `arbitrary_precision`，所以 `trace_id`
是 `1.10` 時兩邊 sample 一致。那句話只在「小數點後的尾隨零」成立 —— 也就是當初唯一
驗過的那個例子。差分 harness 打出來：`1E5` 在 Rust 會變 `1e+5`、`-0` 會變 `0`；而
`json.dumps` 還會把「以字串型態存在的數字」重新加上引號，所以任何值是「內含數字的
物件」的 ID 欄位也一起分岔。這正是「特例推導的公式會連測試一起錯」的形狀：**測試守住
了讀進來那一半，寫出去那一半沒人守**，而錯誤的推廣還被寫進了文件。

**`sample` 政策整個重做成「絕不回吐客戶內容」。** ID 欄位的 needle 是**子字串**比對，
`session_identity_token` 命中 `session_id` —— 而這種欄位的值在很多系統裡本身就是憑證。
多數 API key 比原本 80 字元的上限還短，連截斷都不會發生。既然命中集合是開放的、列舉
不完，唯一安全的規則就是永遠不輸出值：`id_field` 的 sample 改成型別描述
（`string[38]` / `number` / `object[2]` / `array[5]` / `bool`），`uuid_v4` 的 sample
截成 8 字元前綴（v4 形狀的 API key 很常見）。`location` 已經精確到欄位，使用者要動手
修所需的資訊全在那裡。這一刀同時讓整族數字字面值分岔退場，也讓「字元 vs byte 截斷」
變得無關緊要 —— 現在最長的 sample 只有 19 個字元。

**契約違反。** `_scan_value` 在約 1000 層巢狀時會拋 `RecursionError`，直接違反白紙
黑字的「絕不拋例外」—— 而且 `except (ValueError, ...)` 根本接不住。更糟的是 serde_json
在 128 層容器就拒收而 Python 的 parser 不會，於是 127–999 是一整段無聲分岔帶。Python
現在用**迭代**的深度檢查鏡射 serde_json 的上限（用遞迴去量遞迴深度，量到一半就爆掉
你本來要保護的 stack），並由 `depth_126` / `depth_127` 兩側各釘一條。還有三類輸入需要
同樣的對齊：`NaN`、非 UTF-8 與帶 BOM 的 body、落單的 surrogate 跳脫 —— Python 三類都
接受，serde_json 三類都拒收**且是整份文件**，所以拒收的粒度也必須一致。成對 surrogate
必須照常運作，而且有測試 —— 會把該過的一起擋掉的守門不是守門。Rust 那側，`eprintln!`
在 stderr 寫入失敗時會 panic（`proxy 2>&1 | tee log` 的 reader 先退場就是 EPIPE），
偏偏就在那條剛剛宣告過「絕不 panic」的路徑上。

**上限把真發現擠掉了。** `MAX_FINDINGS` 算的是命中次數而非位置，於是三則帶時間戳的
凍結訊息就吃滿十個名額、只覆蓋三個相異位置，並把 `tools` 裡唯一那筆 `session_id`
安靜擠掉。現在上限算相異的 `(kind, location)`、每筆帶 `count`，而 `truncated` 是明
訊號 —— 剛好十筆與「撞上限放棄」不能長得一樣。另加 1 MiB 的掃描預算把工作量圈住：
這條路徑跑在**轉發之前**，掃多久就是延遲多久。

**能抓到這一切的那道 gate 本來不存在。** 17 個 fixture 一個都碰不到那 11 類分岔 ——
「兩份實作互為守門」是一個從未被檢驗過的宣稱。`parity.sh` 因此多了**第三相**：15 個
adversarial fixture，而且每個都有 **golden** 而非只比對兩語言是否一致 —— 因為這裡有
一半的正確答案就是「兩邊都空」，空對空會通過而什麼都沒驗到。這一相另外斷言 fixture
數量，並對每份輸出 grep 埋進去的祕密。閘門：Python 169 → 200、Rust 164 → 191、
parity 17 fixtures × 兩相 + 15 個 adversarial 案例、clippy 0。端到端：把 fixture POST
進跑著的 proxy，印出四行觀測線且轉發的 bytes 不變。

## Run / 執行

```bash
# Python (uv-managed 3.13 venv; fastapi doesn't support 3.14 yet)
cd rewrite && uv run pytest -q              # 200 tests

# Rust (standalone workspace)
cd rewrite/rust-lite && cargo test          # 191 tests

# Cross-language parity gate / 跨語言 parity gate
# 相 1：17 fixtures pipeline bytes；相 2：volatile findings；
# 相 3：15 個 adversarial 案例對 golden（M23 review 後補）
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
