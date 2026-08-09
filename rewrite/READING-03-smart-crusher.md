# 解答本精讀 03 — smart_crusher

> Answer-key deep read #3 — `crates/headroom-core/src/transforms/smart_crusher/`
> （20 個模組、**8,663 行**）。重建無對應物，所以這次不是對照而是學新東西。
>
> 2026-08-09，接續 [READING-01](./READING-01-diff-compressor.md) 與
> [READING-02](./READING-02-log-compressor.md)。屬「讀」非「建」，未改動程式碼。
>
> 規模更正：READING-02 的「尚未讀」清單裡我寫「約 4,500 行」，那是只數了 crusher /
> analyzer / planning / crushers 四個檔。實際是 20 個模組 8,663 行 —— 比四支 compressor
> 加起來還大，是解答本裡最大的單一子系統。

## 它解的是另一個問題

diff / log / search compressor 壓的是**文字**，單位是行。smart_crusher 壓的是
**JSON 記錄陣列** —— 工具回傳 500 筆 row、200 筆搜尋結果、1000 筆事件那種，單位是
陣列裡的一個元素。整個子系統的產出是一份 `CompressionPlan`，裡面是
`keep_indices`：一組要保留的原始索引。

模組分工：

| 模組 | 職責 |
|---|---|
| `classifier` | 陣列元素型別分類（dict / string / number / mixed），決定走哪條 crusher |
| `analyzer` | 統計腦：逐欄位統計，判斷哪個欄位像 ID、哪個像 score |
| `field_detect` | ID-like / score-like 的統計偵測器 |
| `outliers` | 標記「**必須保留**」的項目：結構異常、罕見狀態值、錯誤項 |
| `constraints` | 可插拔的必留規則（`Constraint` trait，OSS 預設含 `KeepErrorsConstraint`） |
| `planning` | 依策略產生 `keep_indices` |
| `orchestration` | 索引集運算，例如「內容相同的多個索引收斂成一個」 |
| `crushers` | 非 dict 陣列（純字串／純數字）的壓法 |
| `statistics` / `stats_math` / `outliers` | 支撐上面所有判斷的統計基礎 |

## 核心落差：按位置選 vs 按資訊選

重建的 JSON 策略（M15）是 `_json_squeeze_core()`：找元素最多的 array，保留
`JSON_HEAD = 5` 個開頭 + `JSON_TAIL = 2` 個結尾，中間全丟。**選擇的依據是位置。**

實測 100 筆 API 健檢結果，97 筆 `ok`、3 筆 `timeout` 埋在第 48–50 筆：

```
原文 6828 bytes → 壓後 550 bytes（省 92%）

三筆 timeout 還在嗎？
  "status": "timeout"            *** 不見了 ***
  upstream did not respond       *** 不見了 ***
  "id": 48                       *** 不見了 ***

留下來的：{"id": 0, ..., "status": "ok", "ms": 42}, {"id": 1, ..., "status": "ok", "ms": 42}, ...
```

模型看到七筆一模一樣的 `ok`，會得出「一切正常」。**唯一值得回報的三筆被壓縮掉了，
而壓縮率 92% 看起來還是一次漂亮的成功。**

這是 READING-02 那個 pytest 缺陷的同構版本，但更危險：那次的失效會讓輸出看起來
可疑（只剩進度點），這次的輸出看起來完全合理。**指標亮綠燈，結論是錯的。**

smart_crusher 對同一份輸入會走完全不同的路徑：`detect_rare_status_values` 認出
`timeout` 是罕見類別值 → 那三筆進 must-preserve 集合 → `keep_indices` 一定包含它們。
選擇的依據是**這一筆帶多少資訊**，不是它排在第幾個。

## 三個重建完全沒有的概念

### 1. 「必須保留」是一等公民，而且可插拔

`Constraint` 是個 trait：

```rust
pub trait Constraint: Send + Sync {
    fn name(&self) -> &str;
    /// Indices of items the allocator MUST keep.
    ...
}
```

必留集合由 constraint 算出來，**與評分完全分離**，再交給 allocator。重建裡沒有
「這一筆不管預算多緊都得留」這個概念 —— 它的策略只有「符合條件的行留、不符的丟」，
沒有第二層優先權。

### 2. 罕見即資訊

`outliers.rs` 的三個偵測器：

- **稀有欄位**：出現在少於 20% 項目裡的欄位 → 帶有它的項目是異常
- **罕見狀態值**：見下節
- **錯誤項**：`error_keywords.rs` 的關鍵字集合

背後的假設很乾淨：**一個值之所以值得看，往往正是因為它跟其他的不一樣。**
均勻抽樣必然錯過它們，而「頭 5 尾 2」是均勻抽樣的退化版本。

### 3. 欄位不是等價的

`analyzer` + `field_detect` 會先判斷「哪個欄位像 ID、哪個像 score」，因為 ID 欄位
逐筆都不同（高基數不代表高資訊），score 欄位的分布才值得取樣。重建的 JSON 策略
不看欄位，它連 JSON 都沒有真的解析（`_scan_arrays` 是掃括號配對）。

## 最好的一段教材：Bug #3

port review 時在 Python 原始碼抓到四個缺陷，其中第三個值得整段抄下來：

```
Python's original guard at smart_crusher.py:674
  if not (2 <= len(unique_values) <= 10): continue
caps cardinality at 10, so error-code domains with 50+ codes are
skipped entirely — even when one or two codes appear at <1% rates
and clearly deserve outlier flagging.
```

**「保留罕見錯誤」這個功能，在錯誤種類一多的時候就自己關掉了。** 而錯誤種類多，
正是最需要它的時候。這是「守門在最該生效的情境下安靜失效」的又一個實例。

修法不是把 10 調大，是換一個從定義推導的判準 —— Pareto 檢查：

1. 基數上限提到 50（超過就幾乎確定是 ID 或自由文字欄，不是狀態列舉）
2. 值的頻率降冪排序，找出最小的 K 使得 top-K 覆蓋 ≥ 80% 的項目
3. 若 K ≤ 5，其餘的值就是「罕見」，含有它們的項目是異常

而且它把三種情況都列出來驗過，包含**不該觸發的那一種**：

| 分布 | 結果 |
|---|---|
| 低基數 + 有主宰值（95×ok + 5 個錯誤） | top-1 覆蓋 95% → 4 個罕見值 → 與舊版同 |
| 較高基數 + 雙峰（60×info + 25×warn + 15 種罕見錯誤） | top-2 覆蓋 85% → 15 個罕見值 → **舊版整個漏掉** |
| 均勻分布（50 個相異值各 2 筆） | K ≤ 5 永遠達不到 80% → 跳過，正確判定為非類別欄 |

「該擋的有擋」與「該過的還會過」兩側都涵蓋 —— 這正是 M21 那批新測試在做的事。

## 四個 port-review 缺陷是同一類

除了 Bug #3，另外三個是：

- **k-split overshoot**：`max(1, round(k_total * fraction))` 對首尾各取 max(1, …)，
  於是 `k_total = 1` 時留下 2 筆，違反 `max_items_after_crush`。
- **sequential-pattern 誤判**：`int("001")` 靜靜吃掉補零，補零字串 ID 被誤判成連號數字 ID。
- **percentile off-by-one**：`len < 8` 時整數除法的百分位索引差一（僅影響 debug 字串）。

四個**沒有一個會拋例外**。它們都是安靜地算錯、安靜地少留、安靜地誤判 ——
只有把兩個實作擺在一起逐位元組比對才會現形。**parity port 在這裡的價值不是「有兩份實作」，
是「兩份實作互為對方的守門」。**

順帶一筆現實：`anchors.rs` 的檔頭寫著這兩個函式在 Python 端被標為 DEPRECATED、
已被 `RelevanceScorer` 取代，但**live path 每次呼叫都還是走它們**，所以照樣得 port。
「標了 deprecated」和「沒人在用」是兩件事。

## 對重建的意義

M21 修好的是「log 策略對某個體裁不啟動」。這次讀到的是更上一層的問題：
**重建的所有策略都在按位置或按逐行條件選擇，沒有任何一支在問「這一筆帶多少資訊」。**

如果要從這裡再往前一步，最小且最有價值的一步不是移植 8,663 行，而是把
「罕見即資訊」這一條放進 JSON 策略：掃一遍最大 array 的某個低基數字串欄位，
把罕見值所在的元素加進必留集合，再套既有的頭尾邏輯。那會是重建第一個
**按資訊選擇**的策略，而且測試很好寫 —— 就是上面那份 97 ok + 3 timeout 的輸入。

## 尚未讀

- `search_compressor.rs`（877 行）
- `cache_stabilization/` 型別矩陣
- `magika_detector.rs` 接線
- smart_crusher 內部尚未細讀：`planning.rs` 四個 planner 的分工、`statistics.rs`
  的欄位特徵化、`compaction` 子模組

---

## English summary

Deep read #3: `smart_crusher/` — 20 modules, **8,663 lines** (I under-counted it as ~4,500 in
READING-02 by only counting four files; it is the single largest subsystem in the answer key).
The rebuild has no counterpart.

It solves a different problem from the text compressors: **statistical compression of JSON
record arrays**, where the unit is an array element and the output is a `CompressionPlan`
carrying `keep_indices`.

**The core gap is positional versus informational selection.** The rebuild's M15 JSON strategy
keeps `JSON_HEAD = 5` leading and `JSON_TAIL = 2` trailing elements of the largest array.
Measured on 100 API health-check records — 97 `ok`, 3 `timeout` buried at indices 48–50 —
it compresses 6828 → 550 bytes (92% saved) and **drops all three timeouts**; what survives is
seven identical `status: ok` rows, from which a model would conclude everything is fine. It is
the same failure shape as the pytest defect in READING-02 but more dangerous, because the
output looks entirely reasonable and the compression ratio looks like a win. `smart_crusher`
would flag `timeout` as a rare categorical value and force those indices into the keep set.

Three concepts the rebuild lacks entirely: **must-preserve as a first-class, pluggable notion**
(the `Constraint` trait, computed separately from scoring); **rarity as information** (rare
fields present in <20% of items, rare status values, error keywords); and **fields are not
equivalent** (ID-like versus score-like detection before any sampling decision).

The best teaching material is bug #3, found during the port: Python's guard
`if not (2 <= len(unique_values) <= 10): continue` **switched off rare-error preservation
exactly when error-code cardinality was high** — that is, when it was most needed. The fix
replaces the magic cap with a Pareto criterion (smallest K whose top-K values cover ≥80%;
rare if K ≤ 5) and enumerates three distributions including the one that must *not* trigger.
All four port-review bugs share a shape: none of them throws. They quietly miscount, quietly
keep too many, quietly misclassify. **The value of the parity port is not having two
implementations — it is that each implementation guards the other.**
