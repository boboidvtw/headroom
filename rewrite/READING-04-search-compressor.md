# 解答本精讀 04 — search compressor

> Answer-key deep read #4 — `crates/headroom-core/src/transforms/search_compressor.rs`
> （877 行）vs 重建的 `SEARCH` 策略（M14，約 70 行）。
>
> 2026-08-09，接續 [01](./READING-01-diff-compressor.md) / [02](./READING-02-log-compressor.md)
> / [03](./READING-03-smart-crusher.md)。四支 compressor 讀完的最後一支。
> 屬「讀」非「建」，未改動程式碼。

## 最重要的發現：只要用了 `rg -C`，重建的 search 策略就整個失效

重建的 `_match_line_key()` 只認 `file:lineno:content`（且 file_key 需含 `/`）。
ripgrep 的 context 行用 `-` 分隔（`src/main.py-40-context`），完全不被認出。

這不是「context 行沒被壓縮」而已 —— **它會讓整個策略拒絕認領**：

```
  rg 旗標        總行數   SEARCH 認領
  rg (無 -C)       80     True
  rg -C 1         240     False
  rg -C 2         400     False
  rg -C 3         560     False
  rg -C 4         720     False
```

端到端實測（10 個檔各 8 筆命中、`rg -C 1`）：

```
SEARCH 認領？ False    → 實際走：truncate
240 行 → 31 行
  保留的命中行:   10 / 80
  涵蓋的檔案數:    2 / 10
```

**十個檔案裡只有兩個活下來。** 而 `rg -C` 是日常用法。

### 根因：比率型閘門的分母裡混進了分類器看不懂的東西

`_search_applies()` 的判準是「可丟行數 / 總行數 >= 0.3」。而「總行數」包含所有行，
包含分類器根本不認得的 context 行：

```
  總行 240，其中被認出的命中行 80，context 行 160（不被認出）
  每檔保留 3 → 可丟 = 10 檔 × (8-3) = 50
  佔比 = 50/240 = 0.208，門檻 0.3  → 不認領

  對照：同一份輸入若沒有 context 行（純 grep -n）
  dropped 50/80 = 0.625 → 認領
```

可丟的行數一模一樣（50），只是分母被灌大了一倍。

**通則：比率型閘門的分母若包含分類器看不懂的東西，未知比例一上升，閘門就會自己關掉。**
這和 READING-03 記的 smart_crusher Bug #3 是同一個家族 —— 那個是
`if not (2 <= len(unique_values) <= 10): continue` 讓「保留罕見錯誤」在錯誤種類一多時
自己關掉。**這已經是本系列第三次遇到「守門在最該生效的情境下安靜失效」。**

## 工業版的答案：解析器錨定在行號標記，不是路徑上

工業版檔頭記了兩個真實世界的 regex bug，值得整段抄：

- **Windows 路徑**：`^([^:]+):(\d+):(.*)$` 對 `C:\Users\foo\bar.py:42:line` 只捕到磁碟機
  代號，`\d+` 隨即失敗 → **每一行 Windows 格式的輸出都被靜靜地從 `file_matches` 丟掉**。
- **檔名含 `-`**：`_RG_CONTEXT_PATTERN` 的 `[^:-]+` 把 dash 排除在路徑之外，於是
  `pre-commit-config.yaml-42-line` 解析錯誤。

修法不是把 regex 補得更複雜，而是換一個錨點：

> Rust parser anchors on the *line-number marker* (`<sep>\d+<sep>`) found
> earliest in the line; everything before is the path, everything after is the content.

**路徑是自由格式的那一段，行號才是有結構的那一段 —— 所以錨定行號。** 這條可以直接搬走：
解析半結構化文字時，錨在受限的欄位上，不要錨在自由欄位上。

重建對這兩個輸入的表現（實測）：

| 輸入 | 重建認得？ |
|---|---|
| `src/utils.py:42:def process(items):` | 是 |
| `C:\Users\dev\proj\utils.py:42:...` | **否** |
| `src/main.py-40-    some context` | **否** |
| `config/pre-commit-config.yaml-42-repos:` | **否** |

誠實的差異：工業版原本的 bug 是**靜靜丟掉**那些行，重建則是**不認得 → 當成非命中行 →
無條件保留**。就資料安全而言重建這邊較保守；代價是它們稀釋了比率閘門（見上節）。
Windows 路徑在 macOS 開發機上實用價值低，`rg -C` 才是真正會咬人的那個。

## 選擇依據：每檔前 3 筆 vs 首尾＋分數最高的中段

重建：`counts[key] > KEEP_PER_FILE` —— 每個檔案保留**前 3 筆**命中。又是位置。

工業版的 pipeline：

1. 解析成 `{file: [(line, content), ...]}`
2. **逐筆命中評分**：context 詞重疊 + `LineImportanceDetector` 的優先訊號 + 設定檔關鍵字
3. 依總分排序檔案、限制 `max_files`
4. 對全域命中清單跑 `compute_optimal_k` 得出自適應總量
5. **每檔選擇：固定保留首/末（可設定），其餘名額依分數填，最後把倖存者排回行號序**
6. 輸出 `file:line:content`，並附 `[... and N more matches in file]` 的**逐檔摘要**

第 5 步的「排回行號序」是個容易忽略的細節：選擇時按分數，輸出時按行號 —— 讀的人
拿到的仍是符合直覺的順序。

第 6 步的逐檔摘要也比重建強：重建只在最末尾放**一個**全域 marker
（`dropped N search result lines`），模型知道「丟了 N 行」但不知道**哪個檔案**還有多少。
工業版逐檔告訴你，於是模型能判斷「這個檔命中 200 筆，值得再撈一次」。

`compute_optimal_k` 在這裡是第三次出現（log、smart_crusher、search）——
**自適應預算在工業版是共用基礎設施，不是某一支的特例。**

## 另外兩處硬化

- **CCR 存取失敗要大聲**：Python 版把 store 的例外全吞了，Rust 版回 `Result` 並
  `tracing::warn!`。與 READING-02 記的 log compressor 同一條。
- **逐檔去重從 O(n²) 降到 O(n log n)**：Python 在迴圈裡做 `match not in file_selected`
  線性查找，Rust 改用 `BTreeSet<(line_number, content_hash)>`。

## 若要從「讀」回到「建」

範圍很小、價值明確的一步：**讓 `_match_line_key()` 認得 `-` 分隔的 context 行**，
判準照工業版錨定行號標記（`<sep>\d+<sep>`，sep 為 `:` 或 `-`），而不是擴充路徑的字元集。
認得之後 context 行就會進入分母與計數，比率閘門不再被稀釋，`rg -C` 的輸出也能走 search
而非落到截斷。

測試要涵蓋兩側：`rg -C 1..4` 都必須認領（現在全 False）、純 grep 仍認領（現在 True，
不得回歸）、以及 log 時間戳 `10:30:45` 仍不得被誤認（M14 原本的 parity 地雷，
`/` 檢查要保留）。

這會是繼 M21（體裁）、M22（選擇依據）之後的第三個「讀出來的修補」，而且三者的病根
是同一個：**判準是為某一種輸入形狀寫的，遇到同類但形狀不同的輸入就安靜地退化。**

## 四支 compressor 讀完的總結

| | 重建的依據 | 工業版的依據 |
|---|---|---|
| diff | 丟掉所有 context | 變更前後各 2 行 + hunk 分數（含 query 詞重疊） |
| log | severity token 詞彙表 | 格式偵測 + 分級評分 + 自適應預算 + 鄰域窗 |
| search | 每檔前 3 筆 | 逐筆評分 + 自適應總量 + 首尾必留 + 排回行號序 |
| JSON/records | 頭 5 尾 2（M22 起加上罕見值） | 統計異常 + 必留約束 + 欄位特徵化 |

四支的共同結構：**重建用一個固定判準把輸入切成留/丟；工業版先量測輸入，再據以配置預算。**

---

## English summary

Deep read #4, the last of the four compressors: the industrial `search_compressor` (877 LOC)
against the rebuild's `SEARCH` strategy (~70 LOC).

**Headline: any use of `rg -C` disables the rebuild's search strategy entirely.** ripgrep
context lines use `-` separators (`src/main.py-40-context`), which `_match_line_key()` does
not recognise. That is not merely "context lines go uncompressed" — the unrecognised lines
inflate the denominator of a ratio gate (`droppable / total >= 0.3`), pushing it below
threshold. Measured: identical droppable count (50), denominator 80 → 240, ratio 0.625 →
0.208, `applies()` flips to false, and the input falls through to blind truncation. On a
10-file `rg -C 1` result, **2 of 10 files survive**. Plain grep works; one line of context
breaks it.

The general principle: **a ratio gate whose denominator includes items the classifier cannot
parse will switch itself off as the unparsed fraction grows.** This is the third instance in
this reading series of a guard that disables itself under the conditions it was built for
(after `smart_crusher`'s bug #3 and the M12/M21 genre mismatch).

The industrial parser fixes two real-world regex failures — Windows drive letters and
filenames containing `-` — not by growing the regex but by changing the anchor: it locates
the *line-number marker* (`<sep>\d+<sep>`) earliest in the line, treating everything before
as path and everything after as content. **Anchor on the constrained field, not the free-form
one** — directly transferable to any semi-structured text parsing.

Selection is by score rather than position (first/last always kept, remaining slots filled by
relevance, survivors re-sorted back to line order), per-file `[... and N more matches in file]`
summaries tell the model *which* file has more rather than a single global count, and
`compute_optimal_k` appears here for the third time — adaptive sizing is shared infrastructure
in the industrial version, not a per-compressor special case.
