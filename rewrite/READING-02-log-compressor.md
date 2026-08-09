# 解答本精讀 02 — log compressor

> Answer-key deep read #2 — `crates/headroom-core/src/transforms/log_compressor.rs` (1,295 lines)
> ＋ `adaptive_sizer.rs` (610 lines) vs the learning rebuild's `LOG` strategy (~60 lines).
>
> 2026-08-09，接續 [READING-01](./READING-01-diff-compressor.md)。同樣屬結案後候選 3
> （「讀」非「建」），未改動任何程式碼。

## 最重要的發現：重建的 log 策略對 pytest / cargo 輸出不會啟動

這不是「落差」，是**缺陷**。M12 的設計註解宣稱：

> 散落在「中段」的 ERROR —— truncate 只留頭尾、會把它們連同中段一起丟掉；
> log 策略逐行嗅探，每個 error 都留下。

實測一份 85 行的 pytest 輸出（頭尾各約 40 行測試進度、中段一個 FAILURES 區塊）：

```
LOG 認領？ False   → 落到 TRUNCATE（盲目頭尾截斷）
壓後 85 → 31 行（head 20 + marker + tail 10）

FAILURES 區塊還在嗎？
  FAILURES               *** 不見了 ***
  AssertionError         *** 不見了 ***
  test_cache.py:42       *** 不見了 ***
```

**M12 宣稱要解決的那個問題，在最常見的真實輸入上原封不動地發生了。**

### 根因

`_severity()` 只認 ASCII 大寫整詞 token：`WARNING/WARN/ERROR/FATAL/CRITICAL`（keep）、
`TRACE/DEBUG/INFO`（drop）。pytest 輸出裡這些一個都沒有 —— 25 行全部分類為 `other`。
於是 `drop == 0`，`_log_applies()` 直接回 `False`，策略拒絕認領。

而 pytest、cargo、jest 表達錯誤的方式根本不是 severity token：

| 工具 | 錯誤長什麼樣 | 命中 `_KEEP_TOKENS`？ |
|---|---|---|
| pytest | `FAILED tests/x.py::test_y`、`E   AssertionError` | 否（`FAILED` ≠ `FATAL`） |
| cargo | `error[E0382]: borrow of moved value` | 否（小寫） |
| jest | `✕ renders correctly`、`● Test suite failed` | 否 |

**這個 token 詞彙表是為「應用程式 runtime log」設計的，卻被套用在「建置/測試輸出」上。**
兩者是不同的體裁。

### 工業版的答案：格式偵測是第 1 步，不是沒有

`log_compressor` 的 pipeline 第一步就是 **format detection（pytest / npm / cargo / jest /
make / generic）**，因為**訊號在哪裡是由格式決定的**，不存在一套通用 token 詞彙表。
重建把這一步整個省略了，等於假設所有 log 都長得像 syslog。

### 誠實的緩解與代價

原文有進 CCR store，模型可以用 M9 的 retrieve 取回 —— 所以這**不是資料遺失**。
但代價是：模型得先知道自己少看了東西、再多花一輪去取。壓縮的目的是讓它一次就看到
重點，而這裡它一次看到的是 79 行測試進度點。

## 第二個發現：預算是「算出來的」，不是常數

重建的所有門檻都是寫死的常數：`HEAD_LINES = 20`、`TAIL_LINES = 10`、`KEEP_PER_FILE = 3`、
`MIN_LOG_LINES = 6`、`LOG_RATIO = 0.6`、`NOISE_RATIO = 0.3`。

工業版的 `compute_optimal_k()`（`adaptive_sizer.rs`，610 行）用**資訊飽和偵測**算出該留幾筆：

1. **快速路徑** — `n <= 8` 全留；simhash 判定唯一群組 ≤ 3 → 只留那幾筆。
2. **Kneedle 拐點** — 對「累積 unique bigram 覆蓋率曲線」找膝點。覆蓋率不再成長的那一點，
   就是「再多留也沒有新資訊」的界線。
3. **zlib 驗證** — 若留下的子集壓縮率比全集冗餘得多，把 k 上調 20%。

外加一個 **diversity floor**：多樣性高（> 0.7）時即使找到膝點也不准壓太狠
（`keep_fraction = 0.3 + 0.7 * diversity`）。

差別的本質：**重建的常數編碼了一個關於「冗餘度」的假設，而且從不量測它；
工業版每次輸入都量一次。** `KEEP_PER_FILE = 3` 對一份高度重複的 grep 結果太多、
對一份每筆都不同的結果太少 —— 而它無從得知自己在哪一邊。

這條和記憶裡「不可重現的增益＝沒有增益」「沒有對照組的『沒退步』等於沒守門」是同一個
家族：**沒有量測的參數，就只是一個沒被證偽過的猜測。**

## 第三個發現：去重，以及去重的危險

重建對 WARN 行是無條件保留 —— 500 條一模一樣的 deprecation warning 會全部留下，
把預算吃光。工業版對 warnings 做 dedupe。

但真正的教材是它「Bug fixes vs Python」裡記的那筆：

> Python's `_dedupe_similar` blanket-normalized digits/paths/hex into single tokens,
> so segfaults at different addresses or test failures with different IDs collapsed
> into a single survivor.

去重必須正規化，而正規化過頭會把**不同的錯誤**合併成一條。修法是保留「訊息前綴」
（第一個 `:` 或 `=` 之前的部分），只把尾端的變動區段 token 化。

**去重的難點不是「找出相同的」，是「定義什麼算相同」** —— 而這個定義錯了會安靜地
吃掉真實的錯誤。

## 第四個發現：stack trace 是「單位」不是「行」

重建沒有 stack trace 的概念，靠「沒有 severity token → 分類為 other → 保留」意外地
留住它們。工業版有 per-flavor 狀態機，而它記的 bug 同樣有教育意義：

> Python's machine terminated on any blank line, dropping mid-trace lines from
> chained-exception traces (which embed blank separators between cause groups).

「空行代表結束」是個看似安全的假設，在 chained exception 上就是錯的。修法是每種語言
各自的終止規則（Python `Traceback` 要在至少一個縮排 frame 之後、遇到非縮排非空行才結束）。

## 第五個發現：選擇必須帶著鄰域（與 READING-01 同一條原則）

工業版的 category selection 在選出 errors / fails / warnings / stack traces / summaries
之後，會對每個選擇**取一個 context window**。這與 diff compressor 保留變更前後 2 行
是同一條原則的兩次實例：

> **被選中的東西單獨拿出來會失去意義；選擇必須連同它的鄰域一起搬。**

重建在兩支策略上都缺這一條（diff 丟光 context、log 逐行留 error 但不帶前後文）。

## 第六個發現：CCR 閘門更嚴

log 的 CCR 條件是 `compression_ratio < 0.5`（省一半以上才存），比 diff 的 0.8 更嚴。
重建兩支都是「丟掉任何一行就 put」。

## 橫貫兩份精讀的一句話

重建的四支策略共用同一個形狀：**手寫偵測器 ＋ 逐行過濾 ＋ 一個常數**。
工業版把這三樣各換成一個「量出來的」東西：

| 重建 | 工業版 |
|---|---|
| 通用 token 詞彙表 | 格式偵測（訊號在哪由體裁決定） |
| 逐行二元過濾 | 分類評分 + 類別選擇 + 鄰域窗 |
| 寫死的常數 | Kneedle 資訊飽和拐點 + 多樣性下限 + zlib 驗證 |

結案時判定的「高原期」在這裡再次得到印證：第 11 支策略還是這個形狀，
而三個欄位的右邊沒有一個是加策略能到達的。

## 一個具體、範圍很小的修補機會（若想從「讀」回到「建」）

`_log_applies()` 目前對 pytest/cargo 回 `False`。最小修補不是擴充 token 表
（那是開放集合，見 READING-01 落差五），而是**在認領條件裡加一條結構性訊號**，
例如 pytest 的 `=== FAILURES ===` / `short test summary info` 區塊標記、cargo 的
`error[E\d+]`。仍是啟發式，但至少讓策略在最常見的輸入上啟動。

要做的話得先補一條測試：**「pytest 輸出經壓縮後，FAILURES 區塊必須存活」** ——
這正是目前 154 條測試裡沒有的那種斷言（現有測試驗的是「策略在它認領的輸入上行為正確」，
沒有驗「它該認領的輸入它有認領」）。

---

## English summary

Deep read #2: the industrial `log_compressor` (1,295 LOC) + `adaptive_sizer` (610 LOC)
against the rebuild's `LOG` strategy (~60 LOC).

**Headline — a real defect, not just a gap.** The rebuild's `LOG` strategy never engages on
pytest/cargo/jest output, the most common tool output in a coding agent. Its severity
detection recognises only ASCII-uppercase whole-word tokens (`ERROR`, `WARN`, `INFO`…),
which build/test runners simply don't emit — pytest says `FAILED`, cargo says
`error[E0382]`. With `drop == 0`, `applies()` returns `False` and the input falls through to
blind head/tail truncation. Demonstrated on an 85-line pytest log: compressed 85 → 31 lines
with the entire `FAILURES` section removed — exactly the mid-log-error loss that M12's design
note claims the strategy prevents. The original is in the CCR store so nothing is lost
permanently, but the model must know to retrieve it. The industrial answer is that **format
detection is pipeline step 1**: where the signal lives is decided by the genre, and no
universal token vocabulary exists.

Other findings: budgets are *computed* (Kneedle knee on a cumulative unique-bigram coverage
curve, simhash near-duplicate fast path, zlib sanity check, diversity floor) rather than
hardcoded; warning dedupe is necessary but its normalisation is dangerous (a documented bug
collapsed distinct segfaults at different addresses into one); stack traces are units with
per-flavour termination rules (blank lines are not a safe terminator for chained
exceptions); selections carry a context window — the same principle as the diff compressor's
±2 lines; and the CCR gate is stricter here (ratio < 0.5 vs diff's 0.8).

Across both reads, the rebuild's strategies share one shape — hand-written detector +
per-line filter + a constant — and the industrial version replaces each with something
measured. That is further evidence for the plateau called at closeout: an 11th strategy has
the same shape, and none of the three replacements is reachable by adding one.
