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

## Run / 執行

```bash
# Python (uv-managed 3.13 venv; fastapi doesn't support 3.14 yet)
cd rewrite && uv run pytest -q              # 154 tests

# Rust (standalone workspace)
cd rewrite/rust-lite && cargo test          # 154 tests

# Cross-language parity gate / 跨語言 parity gate（14 fixtures, byte-for-byte）
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
