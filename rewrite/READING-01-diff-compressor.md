# 解答本精讀 01 — diff compressor

> Answer-key deep read #1 — `crates/headroom-core/src/transforms/diff_compressor.rs` (1,685 lines)
> vs the learning rebuild's `DIFF` strategy (~35 lines) in `src/headroom_lite/strategies.py`.
>
> 2026-08-09。屬結案後候選 3（「讀」非「建」），不改動 `rewrite/` 任何程式碼，
> 四道綠燈（Python 154 / Rust 154 / parity 14 / clippy 0）在讀之前已重跑確認通過。

## 為什麼從 diff 開始

四支 compressor 裡，diff 是唯一在重建中有**逐行對應實作**的一支（M13），因此落差全部來自
設計選擇而非覆蓋率差異 —— 是最乾淨的對照組。log 與 search 在重建裡是簡化版，
smart_crusher 則完全沒有對應物。

## 五個結構性落差

| # | 學習重建 | 工業版 | 性質 |
|---|---|---|---|
| 1 | 丟掉**全部** context 行 | 每個變更保留前後各 2 行 | 資訊取捨 |
| 2 | 6 行以上就壓 | 未滿 50 行原文原樣回；壓縮率未達 20% 不發 CCR key | 成本效益閘門 |
| 3 | 無預算、無排序 | 檔數上限 20（依變更量排序）、每檔 hunk 上限 10（首＋尾＋中段依分數） | 預算配置 |
| 4 | 逐行掃描 | 解析成 files + hunks 結構 | 前三者的前提 |
| 5 | 每支策略自帶 `applies()` 啟發式 | 三層 ContentRouter：magika ML → unidiff → plaintext | 偵測架構 |

## 落差一：context —— 重建丟掉的正是最該留的

重建的 `_diff_squeeze` 是「保留 `+`/`-`、丟掉所有空格開頭的行」。實測（18 行 diff）：

```
@@ -10,12 +10,12 @@ class TokenValidator:
-        payload = jwt.decode(token, self.secret)
+        payload = jwt.decode(token, self.secret, algorithms=["HS256"])

[... headroom-lite dropped 11 diff context lines | sha256:... ]
```

讀者知道改了哪一行，但不知道它在 `validate()` 裡、不知道下一行是 `self._cache[token] = True`。
**離變更最近的 context 資訊密度最高，而重建把它和最遠的填充行一視同仁地丟了。**

工業版 `reduce_context()`（`diff_compressor.rs:964`）以變更行為中心取 `max_context_lines`
半徑的鄰域聯集，並額外硬性保留所有 `\` 開頭的行：

```rust
// Bug-fix: ALWAYS keep `\ No newline at end of file` markers ...
// These are structural patch markers, not context — losing
// them breaks round-trippable patches
```

這條註解點出重建沒有處理的一個維度：**壓縮後的 diff 還能不能當 patch 用**。重建的輸出
碰巧保住了 `\` 開頭的行（它的過濾條件是「不以空格開頭」），但那是巧合不是設計 ——
沒有測試守著，改一次過濾條件就會無聲失去。

## 落差二：成本效益閘門 —— 重建缺的是「不壓」的判斷

工業版預設值（`diff_compressor.rs:114`）：

```rust
max_context_lines: 2,
min_lines_for_ccr: 50,
min_compression_ratio_for_ccr: 0.8,
```

兩道閘門重建都沒有：

- **未滿 50 行原文原樣回。** 檔頭寫明理由：`short diffs that don't benefit from compression
  and would lose context-trim slack`。上面那個 18 行的示範，**工業版根本不會碰它**，
  重建卻壓了 —— 省下 9 行，代價是整段變得難讀。
- **壓縮率未達 20% 不發 CCR key。** 重建只要 `dropped > 0` 就 `store.put()`。存一份原文、
  發一把鑰匙是有成本的，工業版要求這筆成本得換到夠大的節省。

重建的門檻 `MIN_DIFF_LINES = 6` / `DIFF_CONTEXT_RATIO = 0.3` 問的是「**能不能**壓」，
工業版的門檻問的是「**值不值得**壓」。這是兩種不同的問題，而重建從沒問過第二個。

## 落差三：預算配置與相關性排序

工業版在 hunk 數超標時不是截斷，而是**保留首＋尾＋中段分數最高者**，分數由一組
權重常數決定（`SCORE_*`，刻意從 magic number 提升為具名常數）：

- 變更密度基底：`min(0.3, change_count * 0.03)`
- **使用者 query 詞重疊：每命中一個詞 +0.2**（長度 > 2 的詞才算，濾掉 stop word）
- 優先樣式命中：+0.3（每個 hunk 最多加一次）
- 總分上限 1.0

第二項是重建完全沒有的軸：**工業版的壓縮知道使用者在問什麼。** 同一份 diff，問
「為什麼 token 驗證失敗」和問「為什麼 CSS 沒生效」，該留的 hunk 不一樣。重建的
`squeeze(text, store)` 簽章裡根本沒有 query 這個參數 —— 這不是調參能補的，是介面缺一個維度。

值得記一筆的張力：上游自己的 `REALIGNMENT/04-phase-B-live-zone.md` 把
「scoring, relevance」列為要**刪除**的 over-build，但 `diff_compressor` 裡的相關性評分
仍然在。要嘛 Phase B 沒被執行，要嘛這裡的 scoring 被判定為該留的那種。這條沒查證，
留給下次。

## 落差四：結構化解析是前三者的前提

工業版先把輸入解析成 `files → hunks → lines`，落差一到三才有地方掛：沒有 hunk 物件就
沒有「per-hunk 分數」，沒有 file 分組就沒有「檔數上限」。重建的逐行掃描是刻意的簡化
（換來兩語言逐位元組 parity 容易維持），但它同時封死了整個設計空間。

**「簡化實作」與「簡化設計」在這裡分岔**：前者是取捨，後者會讓某些功能變成不可達。

## 落差五：偵測層 —— 重建的 parity 地雷正是 ML 分層要避免的那類 bug

工業版的 `detection.rs` 是三層鏈，且明文寫了「why no regex tier」：

```
Tier 1: magika_detect()       → if non-PlainText, return it
Tier 2: unidiff::is_diff()    → if true, return GitDiff
Tier 3: PlainText (fallthrough)
```

重建則是每支策略自帶 `applies()` 啟發式。而重建自己的註解裡誠實記著一個地雷：

> 若只用「`:` 分隔、第二欄全數字」判 match line，會誤判 log 時間戳 `10:30:45`
> （正是 `欄:數字:欄`）→ search 反吃 log、污染 M12 行為。解法：要求 file_key 含 `/`。

**這正是 magika 那層存在的理由。** 手寫啟發式的失效模式是「另一種內容碰巧長得像」，
而這是個開放集合 —— 修好 `10:30:45` 不代表下一個不會出現。這條和記憶裡
「黑名單要擋『取得參照』而非列舉『所有呼叫寫法』（後者是開放集合）」是同一個形狀。

## 這次精讀對「高原期」判定的意義

結案時判定建構面進入高原期，理由是 dispatcher 從 M12 到 M20 十度驗證同一套
`(applies, squeeze)` 套路。這次精讀支持那個判定，但也指出**高原的出口不在第 11 支策略**：

- 落差三與五都不是「再加一支策略」能達到的，它們要改介面（squeeze 要拿得到 query）
  與改架構（applies 換成學習式偵測）。
- 落差二甚至不用改架構 —— 「值不值得壓」的閘門是純加法，且直接對應記憶裡
  「守門要問這條斷言如果功能沒作用會不會照樣通過」的同一種思考。

## 尚未讀

- `log_compressor.rs`（1,295 行）：格式偵測（pytest/npm/cargo/jest/make）＋逐行分級評分
  ＋自適應總行數預算。重建的 log 策略只有嚴重度分類。
- `search_compressor.rs`（877 行）
- `smart_crusher/`（crusher + analyzer + planning + crushers 約 4,500 行）：重建無對應物
- `cache_stabilization/` 型別矩陣
- `magika_detector.rs` 的實際接線

---

## English summary

Deep read of the industrial `diff_compressor` (1,685 LOC) against the learning rebuild's
`DIFF` strategy (~35 LOC). Five structural gaps, in rough order of how hard they are to close:

1. **Context.** The rebuild drops *all* context lines; the industrial version keeps a
   ±2-line neighbourhood around each change and always preserves `\ No newline` markers so
   the output stays a usable patch. The rebuild discards exactly the highest-value context.
2. **Cost/benefit gates.** `min_lines_for_ccr: 50` (short diffs pass through untouched) and
   `min_compression_ratio_for_ccr: 0.8` (no CCR key unless ≥20% saved). The rebuild asks
   "*can* this be compressed", never "*is it worth* compressing".
3. **Budgeting and relevance.** File/hunk caps with first+last+top-scored-middle selection,
   scored partly by **overlap with the user's query** — an axis absent from the rebuild's
   `squeeze(text, store)` signature entirely.
4. **Structured parse** (files → hunks → lines) is the precondition for 1–3.
5. **Detection.** A three-tier ContentRouter (magika ML → unidiff → plaintext) instead of
   per-strategy hand-written `applies()` heuristics. The rebuild's own documented parity
   landmine — grep-output detection misfiring on log timestamps `10:30:45` — is precisely
   the open-set failure mode the ML tier exists to avoid.

The plateau called at closeout holds, but the exit from it is not an 11th strategy: gaps 3
and 5 require interface and architecture changes, while gap 2 is pure addition.
