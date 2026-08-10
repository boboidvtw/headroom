# 解答本精讀 05 — cache stabilization

> Answer-key deep read #5 — `crates/headroom-proxy/src/cache_stabilization/`
> （5 個模組、3,276 行）vs 重建的 `cache_stabilization.py`（M3，146 行）。
>
> 2026-08-09，接續 [01](./READING-01-diff-compressor.md)–[04](./READING-04-search-compressor.md)。
> 四支 compressor 之外的第一個子系統。屬「讀」非「建」，未改動程式碼。

## 架構落差：observe 與 normalize 是兩件事

工業版 `mod.rs` 開宗明義把 Phase E 的機制切成兩類：

| 類別 | 模組 | 會不會改 request bytes |
|---|---|---|
| **Observe** | `volatile_detector`(E5)、`drift_detector`(E6) | **絕不** |
| **Normalize** | `tool_def_normalize`(E1/E2)、`anthropic_cache_control`(E3)、`openai_cache_key`(E4) | 會，但受閘門管 |

**重建只有 normalize 那一半**（E1/E2/E3），observe 整個沒有，E4 也沒有。

這個切分不是分類上的潔癖，它對應到一個真實的分工：

> **正規化修的是 proxy 修得動的；觀測揭露的是只有客戶自己能修的。**

客戶的 system prompt 裡塞了一個每次請求都不同的時間戳 —— proxy 不能替他刪（那是他的
內容），但可以告訴他「你的快取前綴裡有這個東西」。重建沒有這一半，所以遇到這種情況
它只能沉默。而從外面看，**「因為客戶內容易變而 miss」和「因為 proxy 弄壞而 miss」
長得一模一樣** —— 偵測器就是用來分辨這兩者的。

## 重建其實幾乎每次請求都在改寫 tools

`_normalize_tools()` 會遞迴排序 `input_schema` 的所有 key。而人寫 JSON Schema 的
自然順序是 `type` 在最前面，字母序卻是 `properties < required < type`：

```
原始 input_schema key 順序: ['type', 'properties', 'required']
正規化後                  : ['properties', 'required', 'type']
→ stabilize 改 bytes 嗎？ 有
```

也就是說：**只要 client 的 schema key 不是字母序（幾乎是所有人），每一次請求的
tools 陣列都會被重寫。**

四種組合實測：

| tools 已排序 | client 有標記 | stabilize 改 bytes | 原因 |
|---|---|---|---|
| 是 | 是 | 否 | — |
| 是 | 否 | 否 | （此例無 system/歷史訊息，E3 無處可放） |
| 否 | 是 | **是** | tools 排序 |
| 否 | 否 | **是** | 排序 + E3 補標記 |

### 這與本專案自己的 M8 發現有張力

M8 的 wrap Claude Code 實測（見 `journal/2026-06-12_headroom_cc_wrap_cache_investigation.md`）
記錄了：**改動 `tools` 就讓 API 對 raw 流量的 ~30k 部分命中容錯失效**。當時的結論把
stabilize 排除在外，理由是「stabilize 是 no-op」—— 對 Claude Code 的流量而言確實如此
（CC 的 schema key 顯然已是字母序）。

**風險沒有被觀察到，不是因為它不存在，而是因為當時測的那一個 client 剛好不會觸發它。**
這和本系列反覆遇到的形狀一樣：一個檢查通過了，而通過的理由不能推廣。

誠實的界線：M8 的證據是關於**附加一個 tool**（`register_ccr_tool`），
**重排既有 tools 是否有同樣效果並未被驗證**。這是一個值得測的假設，不是已成立的缺陷。
而且測法現成 —— M8 的 register-only pair 重放手順可以直接改成 reorder-only pair。

### 工業版怎麼處理同一個風險

兩道重建沒有的閘門：

1. **auth-mode 閘門**：正規化只在 PAYG 模式生效，**OAuth 與 Subscription 一律 passthrough**。
   對訂閱制 CLI 動 bytes 是指紋辨識風險（Phase F 的主題），所以乾脆不動。
   重建無條件正規化。
2. **標記前提**：PR-E1 在「任何 tool 已帶 top-level `cache_control`」時跳過 —— 客戶已經
   表達了意圖就不要插手。

值得學的是第 2 條的**反面**：PR-E2（schema key 排序）**刻意沒有**標記檢查，而且寫明理由 ——
排序 schema 內部的 key 永遠不會移動標記，因為標記掛在 tool 物件上、不在 schema 裡面。
**把「這裡為什麼不需要守門」寫下來，和寫守門一樣有價值**；否則下一個人會把它當成漏掉的。

## Observe 側：兩個偵測器

### volatile_detector（E5）— 找出會炸掉快取的內容

掃三類東西，全部唯讀：

1. **ISO-8601 時間戳** —— 幾乎都是每次請求現算的，前綴含它的快取命中是意外。
2. **UUID v4** —— 用第 14 位的 version nibble 分辨「呼叫端每次現產的 UUID」與
   「隨機十六進位字串」。build hash 通常不是 v4，固定識別碼則根本不會變。
   **這個判準很漂亮：它不是在找 UUID，是在找「看起來像每次都會變的 UUID」。**
3. **ID 名稱的欄位** —— `request_id` / `trace_id` / `session_id` / `correlation_id`。
   補的是前兩條漏掉的：整數 trace ID、自訂 slug 格式。

檔頭明確立下 **non-mutation invariant**，並說明它由呼叫端的 `debug_assert_eq!` 長度檢查
與整合測試的 SHA-256 逐位元組相等來守住 —— **不變量有人守，不是只寫在註解裡**。

### drift_detector（E6）— 指出是哪一個維度漂移了

對「快取熱區」算三個獨立的 SHA-256：`system`、`tools`、`early_messages`（前 3 則）。
用有界 LRU 記住每個 session 的上一次雜湊，不一致時記錄**是哪些維度漂了**。

兩個細節值得記：

- **刻意跳過 live-zone 尾端**，因為那裡本來就會變、變了也無害。
  這與 smart_crusher 的必留約束是同一個思路的另一面：**先分清「哪些變化是預期的」，
  才有辦法對「非預期的變化」發警報**。否則每次請求都在漂，警報等於雜訊。
- **隱私**：session key 取最強的可用識別碼（`Authorization` → `x-api-key` → client IP →
  `(ip, user_agent)`），而 bearer token 與 API key **在離開這個模組之前就先雜湊**，
  原始祕密不記錄、不儲存，log 裡只有雜湊的短前綴。

## 對重建的意義

前四份精讀談的都是「壓縮壓得好不好」。這一份談的是**壓縮以外的那件事**：
proxy 站在流量中間，除了改東西，還可以**看見東西並告訴使用者**。

重建的 M3 註解寫著「從『不搞砸 cache』進階到『主動幫 client 提高命中率』」。
讀完工業版才看清楚，「主動幫忙」其實有兩條路，而重建只走了會改 bytes 的那一條 ——
偏偏那條風險比較高，另一條零風險。

若要從「讀」回到「建」，這裡有一個**零風險、且不需要 Phase F 概念**的候選：
在 pipeline 加一個唯讀的 volatile 掃描，發現快取前綴裡有 ISO 時間戳或 v4 UUID 就記一行
觀測線。它不改 bytes（parity 不受影響、既有 fixture 全部不動），卻補上重建完全缺席的
半邊。測試好寫：塞一個帶時間戳的 system prompt 進去，斷言「有被指出來」，
以及「乾淨的 prompt 不得誤報」。

比它更該先做的則是那個張力的實測：**用 M8 的重放手順，測「只重排 tools、不附加」
會不會同樣打掉部分前綴命中**。那個答案會決定 M3 的正規化該不該加閘門。

## 尚未讀

- `magika_detector.rs` 接線（READING-01 落差五的那個 ML 偵測層）
- `openai_cache_key.rs`（E4，重建無對應物）
- smart_crusher 內部：`planning.rs` 四個 planner、`statistics.rs`、`compaction`

---

## English summary

Deep read #5: the industrial `cache_stabilization/` (5 modules, 3,276 LOC) against the
rebuild's `cache_stabilization.py` (M3, 146 LOC).

**The architectural gap is the observe/normalize split.** The industrial module divides Phase
E into observers that *never* mutate request bytes (`volatile_detector`, `drift_detector`) and
normalizers that do, behind gates (`tool_def_normalize`, `anthropic_cache_control`,
`openai_cache_key`). The rebuild has only the normalizing half. The division of labour matters:
normalization fixes what the proxy can fix, observation surfaces what only the customer can
fix — and from the outside, a cache miss caused by the customer's volatile content is
indistinguishable from one caused by the proxy.

**A measured finding:** the rebuild rewrites the `tools` array on essentially every request,
because recursive schema-key sorting reorders the natural human ordering (`type` first;
alphabetically `properties < required < type`). This sits in tension with the project's own M8
investigation, which found that modifying `tools` destroyed the API's ~30k partial-prefix-match
tolerance — an investigation that explicitly excluded `stabilize` because it was a no-op *for
Claude Code traffic*. The risk went unobserved not because it is absent but because the one
client tested happened not to trigger it. Being precise: M8's evidence concerns *appending* a
tool; whether *reordering* has the same effect is untested. It is a hypothesis worth measuring,
and M8's replay procedure adapts to it directly.

The industrial version guards the same risk two ways the rebuild does not: normalization is
**PAYG-only** (OAuth and Subscription always pass through, because mutating bytes for
subscription CLIs is a fingerprinting risk), and PR-E1 skips when the customer has already
placed a `cache_control` marker. Worth stealing is the *inverse*: PR-E2 deliberately has **no**
marker check and says why — sorting keys inside a schema can never move a marker that lives on
the tool object. Documenting why a guard is unnecessary is as valuable as writing one.

On the observe side, `volatile_detector` looks for ISO-8601 timestamps, UUID v4s — using the
version nibble to distinguish per-request UUIDs from build hashes, i.e. it is not looking for
UUIDs but for UUIDs *that will change* — and ID-named fields, with a stated non-mutation
invariant gated by byte-equality tests. `drift_detector` fingerprints the cache hot zone in
three independent dimensions, deliberately skipping the live-zone tail where change is expected
and benign, and hashes bearer tokens before they leave the module.

A zero-risk next build for the rebuild: a read-only volatile scan that emits an observation line
when the cached prefix contains a timestamp or a v4 UUID. It cannot affect parity or any
existing fixture, and it fills in the half of Phase E that is entirely absent.
