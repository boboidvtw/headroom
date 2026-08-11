"""M23 — volatile 內容唯讀掃描（Phase E 的 observe 那一半）。

M3 做的是 normalize：改 client 的 bytes 讓前綴收斂。這片做的是 observe：
**一個 byte 都不改**，只指出「你的快取前綴裡有每次請求都會變的東西」。

為什麼兩者不能互相取代（READING-05）
------------------------------------
> 正規化修的是 proxy 修得動的；觀測揭露的是只有客戶自己能修的。

client 的 system prompt 裡塞了一個每次現算的時間戳 —— proxy 不能替他刪
（那是他的內容），但可以告訴他。沒有這一半，proxy 遇到這種情況只能沉默；
而從外面看，**「因為客戶內容易變而 miss」和「因為 proxy 弄壞而 miss」
長得一模一樣**，偵測器就是用來分辨這兩者的。

掃三類（全部唯讀）
------------------
  1. ISO-8601 時間戳 —— 幾乎都是每次請求現算的，前綴含它還能命中是意外。
  2. UUID v4 —— 用第 14 位的 version nibble 分辨「呼叫端每次現產的 UUID」
     與「隨機十六進位字串」。**判準的精髓不是找 UUID，是找「看起來每次
     都會變的 UUID」** —— build hash 通常不是 v4，固定識別碼根本不會變。
  3. ID 名稱的欄位（request_id / trace_id / session_id / correlation_id）
     —— 補前兩條漏掉的：整數 trace ID、自訂 slug 格式。

sample 政策：絕不回吐客戶內容（2026-08-10 code review 後改）
-----------------------------------------------------------
初版照解答本把命中的值原文放進 sample，最長 80 字元。review 實測打穿：
needle 是**子字串**比對，`session_identity_token` 命中 `session_id`，於是
一把 40 字元的 API key 原封不動進了 stderr log —— 而多數 key 根本不到 80
字元、連截斷都不會發生。`session_id` 這種欄位在很多系統裡**本身就是憑證**。

現在的政策：

  ============  ========================================================
  kind          sample
  ============  ========================================================
  timestamp     命中的 19 字元原樣（時間戳不可能是祕密）
  uuid_v4       前 8 字元 + `…`（v4 形狀的 API key 很常見）
  id_field      **只給型別**：`string[42]` / `number` / `object[3]` /
                `array[5]` / `bool` —— 永遠不含客戶的值
  ============  ========================================================

使用者要修的資訊在 `location` 裡（它已經精確到欄位），值長什麼樣不影響
他要做的事；賣掉的風險卻很大。

這一刀同時解掉一整族 parity 分岔：sample 不再渲染任意值，所以
`json.dumps` 幫數字加引號、`arbitrary_precision` 把 `1E5` 正規化成 `1e+5`、
`-0` 變 `0`、以及「字元 vs byte 截斷」全部消失。**初版 docstring 宣稱
`parse_float=str` 對齊 `arbitrary_precision`，那句話是錯的** —— 它只在
「小數點後尾隨零」成立（我只驗了 `1.10` 一個例子就推廣了），指數形式與
負零都會分岔。現在數字根本不渲染，問題不存在。

刻意偏離解答本之處（各有理由，不是漏做）
----------------------------------------
  a. **入口吃 bytes、自己 parse 一份副本**。工業版收 `&Value`（proxy 那層
     已經 parse 過，省一次 JSON 成本），非變性靠呼叫端的 debug_assert 與
     整合測試守。重建反過來：多付一次 parse，換「掃描器手上根本沒有呼叫端
     的物件」—— 非變性從『有測試守著』升級成『結構上不可能違反』。
  b. **sample 不回吐客戶值**（見上）。工業版會，這是重建**嚴格更保守**的
     一處。工業版另有「bearer token 離開模組前先雜湊」的機制，重建沒有；
     不回吐值是達到同一目的的更粗但更完整的作法。
  c. **沒有 ApiKind 分歧**。工業版要同時走 Anthropic 與 OpenAI 兩種 body
     形狀；重建全程只有 `/v1/messages`，多一個 enum 只是憑空的分支。
     —— 承 READING-05 的教訓：把「這裡為什麼不需要」寫下來，和寫守門
     一樣有價值，否則下一個人會把它當成漏掉的。

policy（承襲工業版）
--------------------
  - **不用 regex**：每個 pattern 都是明寫的位元組位置檢查，意圖看得見。
  - **上限 10 個相異 (kind, location)**，超過時 `truncated` 為真。上限算
     的是相異位置不是命中次數 —— 同一段 log 裡的 40 個時間戳只佔一個名額，
     否則它會把 tools 裡真正該報的東西安靜擠掉（review 實測重現）。
  - **`truncated` 是明訊號**：剛好 10 筆與「≥10、我們放棄了」不能長得一樣。
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field

# kind 的字串表示是穩定介面（parity 報告與觀測線都吃它），別隨手改。
TIMESTAMP = "iso8601_timestamp"
UUID_V4 = "uuid_v4"
ID_FIELD = "id_field"

MAX_FINDINGS = 10

#: UUID sample 只留前綴，足以定位而不構成可用的憑證。
UUID_SAMPLE_CHARS = 8

#: 單一 location 片段（客戶的 key 名）的字元上限。
#: location 是**客戶自己的 key 名串起來的路徑**，祖先 key 完全不受 needle 約束
#: —— 一份用 email 或 token 當 map key 的 JSON，整串都會進觀測線。sample 政策
#: 用來否決「回吐值」的理由（needle 是子字串比對、命中集合是開放集合、不能假設
#: 命中的東西不是祕密）對 key 名同樣成立。
#: 但 location 是這筆 finding **唯一可行動的內容**，拿掉它等於拿掉整筆發現，
#: 所以這裡的處置是**設界**而不是消除：限制單段與總長，同時擋住 log 體積 DoS。
MAX_LOCATION_SEGMENT_CHARS = 40
MAX_LOCATION_CHARS = 200

#: 容器巢狀深度上限。**這個數字必須跟 Rust 對齊**：serde_json 的 parser
#: 在第 128 層容器就回 Err（整份文件 → 空結果），Python 的 json 沒有這個
#: 限制，於是 127–999 層會變成一整段無聲分岔帶（review 實測）。兩側各由
#: `depth_126` / `depth_127` 兩個 adversarial fixture 釘住 —— 只釘一側的話
#: 另一側漂了不會有人知道。
MAX_NESTING = 127

#: 超過這個大小就整包放棄掃描（`truncated` 為真）。觀測是盡力而為的功能，
#: 不值得為一份 1 GB 的 body 佔住 proxy 的 worker thread —— 而且這條路徑
#: 跑在**轉發之前**，掃多久就是延遲多久。
MAX_SCAN_BYTES = 1 << 20

# 慣例上「每請求唯一」的 JSON key 名。對 key 做 ASCII 小寫後的子字串比對
# —— `x_request_id`、`meta_session_id` 都認得。
# 注意這是**開放集合**：`session_identity_token` 也會命中。這正是 sample
# 不准回吐值的理由 —— 命中集合列舉不完，就不能假設命中的東西不是祕密。
_ID_FIELD_NEEDLES = ("request_id", "trace_id", "session_id", "correlation_id")

_ISO_LEN = 19  # `YYYY-MM-DDTHH:MM:SS`
_UUID_LEN = 36


@dataclass(frozen=True)
class VolatileFinding:
    """一筆發現。

    location 是欄位存取路徑（`system[2].text`、
    `tools[0].input_schema.properties.session_id`），讓使用者把警告對回
    自己 request 裡的確切欄位。count 是同一個 (kind, location) 的命中次數。
    sample 見模組 docstring 的 sample 政策 —— **永不含 id_field 的值**。
    """

    kind: str
    location: str
    sample: str
    count: int = 1


@dataclass(frozen=True)
class VolatileScan:
    """掃描結果。

    truncated 為真代表「撞到 MAX_FINDINGS，還有更多沒列出」—— 沒有這個
    欄位的話，剛好 10 筆與撞上限長得一模一樣，而那正是本專案反覆吃虧的形狀
    （守門在最該生效時安靜失效）。

    skipped_too_large 是**另一件事**：body 超過 MAX_SCAN_BYTES，根本沒掃。
    初版把兩者共用 `truncated`，於是 proxy 會對一份沒掃過的 body 印出
    「已達 10 個相異位置的上限」—— 修掉了第一層歧義，又在同一個欄位上長出
    第二層。訊號要能分辨，就不能一號多用。
    """

    findings: list[VolatileFinding] = field(default_factory=list)
    truncated: bool = False
    skipped_too_large: bool = False

    def __iter__(self):
        return iter(self.findings)

    def __len__(self) -> int:
        return len(self.findings)

    def __getitem__(self, index):
        return self.findings[index]


# ─── 入口 ──────────────────────────────────────────────────────────────


def scan_request(raw: bytes) -> VolatileScan:
    """掃描 request body bytes。永不修改任何東西、永不拋例外。

    看不懂的輸入一律回空結果 —— 與 M0 起的失敗模式契約一致：絕不因為觀測
    而影響請求。這裡的「看不懂」刻意與 **Rust 端 serde_json 的判準對齊**，
    因為兩邊不一致就是無聲分岔（以下每一條都是 review 用差分 harness 打
    出來、且各有一個 adversarial fixture 釘住的）：

      * 非 UTF-8：`json.loads(bytes)` 會依 BOM 與 null byte 模式自動偵測
        UTF-16/32，`serde_json::from_slice` 只吃 UTF-8 → 先顯式 decode。
        （UTF-8 BOM 能 decode，但留下的 `\\ufeff` 會讓 json.loads 失敗，
        與 Rust 一致。）
      * `NaN` / `Infinity`：Python 預設接受，serde_json 一律 Err。而且
        Rust 是**整包**失敗，body 裡其他地方的 findings 會一起消失 →
        `parse_constant` 拒收。
      * 落單的 surrogate 跳脫（`\\ud800`）：Python 接受並產生無法編碼成
        UTF-8 的 str，serde_json 要求成對 → 先掃原始文字擋掉。成對的
        （emoji）必須照常通過。
      * 容器巢狀 > MAX_NESTING：對齊 serde_json 的 parse 深度上限。
    """
    if len(raw) > MAX_SCAN_BYTES:
        return VolatileScan([], truncated=False, skipped_too_large=True)
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return VolatileScan()
    if _has_lone_surrogate_escape(text):
        return VolatileScan()
    try:
        body = json.loads(text, parse_constant=_reject_constant)
    except (ValueError, RecursionError):
        return VolatileScan()
    if not isinstance(body, dict):
        return VolatileScan()
    if _nesting_depth(body) > MAX_NESTING:
        return VolatileScan()
    return detect_volatile_content(body)


def detect_volatile_content(body: dict) -> VolatileScan:
    """走訪 Anthropic `/v1/messages` 形狀的快取熱區。唯讀。

    走訪順序固定（system → messages → tools）且 dict 依插入順序 ——
    Rust 端開了 preserve_order，兩邊撞到上限時砍掉的是同一批。
    """
    out = _Accumulator()

    system = body.get("system")
    if system is not None:
        _scan_content(system, ["system"], out, 1)

    messages = body.get("messages")
    if isinstance(messages, list):
        # **最後一則不掃**（刻意偏離解答本的 E5，它掃全部 messages）。
        #
        # 快取前綴的邊界由 M3 的 `_place_breakpoints` 定義：標記 2 放在
        # `messages[-2]`，所以 `messages[-1]` 從來就不在前綴裡 —— 那是
        # live zone，它每輪都變、變了無害，也正是壓縮引擎接著要改寫的東西。
        #
        # （同 drift_detector「先分清哪些變化是預期的，才有辦法對非預期的
        #   變化發警報」；否則每次請求都在漂，警報等於雜訊。）
        for i, message in enumerate(messages[:-1]):
            if out.full:
                return out.result()
            if isinstance(message, dict) and "content" in message:
                _scan_content(message["content"], [f"messages[{i}].content"], out, 1)

    tools = body.get("tools")
    if isinstance(tools, list):
        for i, tool in enumerate(tools):
            if out.full:
                return out.result()
            if not isinstance(tool, dict):
                continue
            description = tool.get("description")
            if isinstance(description, str):
                _scan_string(description, [f"tools[{i}].description"], out)
            if "input_schema" in tool:
                _scan_value(tool["input_schema"], [f"tools[{i}].input_schema"], out, 1)

    return out.result()


class _Accumulator:
    """依 `(kind, location)` 去重的收集器。

    上限算的是**相異位置**而非命中次數。初版兩者不分，review 實測：三則
    含時間戳的凍結歷史就吃滿 10 個名額（只覆蓋 3 個相異位置），把 tools
    裡唯一真正該報的 `session_id` 完全擠掉 —— 噪音之外還會漏報。
    """

    def __init__(self) -> None:
        self._counts: dict[tuple[str, str], int] = {}
        self._samples: dict[tuple[str, str], str] = {}
        self.truncated = False

    def add(self, kind: str, path: list[str], sample: str) -> None:
        location = _render_location(path)
        key = (kind, location)
        if key in self._counts:
            self._counts[key] += 1
            return
        if len(self._counts) >= MAX_FINDINGS:
            self.truncated = True
            return
        self._counts[key] = 1
        self._samples[key] = sample

    @property
    def full(self) -> bool:
        """撞上限後就停止走訪（別為了數重複而掃完整份 body）。"""
        return self.truncated

    def result(self) -> VolatileScan:
        return VolatileScan(
            [
                VolatileFinding(kind=k, location=loc, sample=self._samples[(k, loc)], count=n)
                for (k, loc), n in self._counts.items()
            ],
            truncated=self.truncated,
        )


# ─── 走訪 ──────────────────────────────────────────────────────────────


def _scan_content(value, path: list[str], out: _Accumulator, depth: int) -> None:
    """content 位置：可能是字串、可能是 block 陣列、也可能是 object。

    行為與 `_scan_value` 等價（前者的 object 分支直接委派後者）；保留兩個
    名字只為與解答本的 `scan_value_for_strings` / `scan_value_recursive`
    結構對齊，Rust 端同樣拆分。
    """
    if out.full:
        return
    if isinstance(value, str):
        _scan_string(value, path, out)
    elif isinstance(value, list):
        for i, item in enumerate(value):
            if out.full:
                return
            path.append(f"[{i}]")
            _scan_value(item, path, out, depth + 1)
            path.pop()
    elif isinstance(value, dict):
        _scan_value(value, path, out, depth)


def _scan_value(value, path: list[str], out: _Accumulator, depth: int) -> None:
    """唯一會檢查 **key 名稱** 的走訪器：tool input_schema、巢狀 block
    都流經這裡。

    `path` 是**可變的片段堆疊**，push/pop 而不是每個節點都串一條新字串。
    初版對每個 key、每個陣列元素都無條件 `format!` 一條 location，即使整份
    body 一個 finding 都沒有 —— review 實測 1 MiB 零 findings 的深結構
    body 要 114 ms / 166 MB，而同位元組數的淺結構只要 1.6 ms / 4.3 MB。
    `MAX_SCAN_BYTES` 限的是位元組不是衍生工作量，那個洞正好從它底下鑽過去。
    location 現在只在**真的產生 finding 時**才具體化。

    depth 守門：`scan_request` 已在文件層擋掉過深的輸入，但
    `detect_volatile_content` 是公開 API、可以直接收手工建的結構 ——
    Rust 那側在這種情況會 stack overflow 而 **abort（連攔都攔不到）**，
    所以兩邊都在走訪內再擋一次。對通過文件層檢查的輸入永不觸發。
    """
    if out.full or depth > MAX_NESTING:
        return
    if isinstance(value, str):
        _scan_string(value, path, out)
    elif isinstance(value, list):
        for i, item in enumerate(value):
            if out.full:
                return
            path.append(f"[{i}]")
            _scan_value(item, path, out, depth + 1)
            path.pop()
    elif isinstance(value, dict):
        for key, sub in value.items():
            if out.full:
                return
            path.append("." + _cap_segment(key))
            if _is_id_named_key(key) and not _is_value_empty(sub):
                out.add(ID_FIELD, path, _describe(sub))
            _scan_value(sub, path, out, depth + 1)
            path.pop()


def _scan_string(text: str, path: list[str], out: _Accumulator) -> None:
    """在一段字串裡找時間戳與 UUID v4。同一段字串裡多次命中累加 count。

    這裡逐 **code point** 掃，Rust 端逐 **byte** 掃 —— 兩邊的索引不同，
    但兩個 pattern 都是純 ASCII，所以認出來的是同一個子字串、跳過的也是
    同一段（19 個 ASCII 字元 == 19 bytes）。而且我們只回報子字串本身、
    從不回報偏移量，findings 因此逐字對齊。（M15/M20 的 native-index 範式。）
    """
    n = len(text)
    i = 0
    while i < n:
        if out.full:
            return
        # 先試 ISO-8601：視窗較短，字串剛好在 UUID 中間結束時比較不會漏。
        if i + _ISO_LEN <= n and _looks_like_iso8601(text, i):
            out.add(TIMESTAMP, path, text[i : i + _ISO_LEN])
            i += _ISO_LEN
            continue
        if i + _UUID_LEN <= n and _looks_like_uuid_v4(text, i):
            out.add(UUID_V4, path, text[i : i + UUID_SAMPLE_CHARS] + "…")
            i += _UUID_LEN
            continue
        i += 1


# ─── location 設界 ────────────────────────────────────────────────────


def _cap_segment(key: str) -> str:
    """限制單一 key 名的長度。

    副作用要知道：兩個只在第 41 字元之後才不同的 key 會被折成同一個
    location，於是被 `_Accumulator` 併成一筆（count 累加）。這是刻意的
    取捨 —— 觀測線的可讀性與長度上限，優先於區分兩個 40 字元前綴相同的 key。
    """
    if len(key) <= MAX_LOCATION_SEGMENT_CHARS:
        return key
    return key[:MAX_LOCATION_SEGMENT_CHARS] + "…"


def _render_location(path: list[str]) -> str:
    """把片段堆疊串成 location，並限制總長。

    保頭也保尾：頭部是 `system` / `tools[0]` 這類定位資訊，尾部是真正命中
    的欄位名 —— 兩端都比中間有用，所以中段省略。
    """
    joined = "".join(path)
    if len(joined) <= MAX_LOCATION_CHARS:
        return joined
    keep = (MAX_LOCATION_CHARS - 1) // 2
    return joined[:keep] + "…" + joined[-keep:]


# ─── pattern 判別（全部明寫位置，不用 regex）────────────────────────────


def _is_ascii_digit(c: str) -> bool:
    """**只認 ASCII `0-9`**。不能用 `str.isdigit()` —— 它對 `٢`（阿拉伯-印度
    數字）也回 True，而 Rust 的 `u8::is_ascii_digit` 不會，兩邊會分岔。"""
    return "0" <= c <= "9"


def _is_ascii_hex(c: str) -> bool:
    return _is_ascii_digit(c) or "a" <= c <= "f" or "A" <= c <= "F"


def _looks_like_iso8601(s: str, at: int) -> bool:
    """`YYYY-MM-DDTHH:MM:SS`：4=`-`、7=`-`、10=`T`/`t`/空格、13=`:`、16=`:`，
    其餘是 ASCII 數字。（RFC 3339 §5.6 允許用空格代替 `T`。）"""
    if not all(_is_ascii_digit(s[at + k]) for k in (0, 1, 2, 3)):
        return False
    if s[at + 4] != "-" or s[at + 7] != "-":
        return False
    if not all(_is_ascii_digit(s[at + k]) for k in (5, 6, 8, 9)):
        return False
    if s[at + 10] not in ("T", "t", " "):
        return False
    if s[at + 13] != ":" or s[at + 16] != ":":
        return False
    return all(_is_ascii_digit(s[at + k]) for k in (11, 12, 14, 15, 17, 18))


def _looks_like_uuid_v4(s: str, at: int) -> bool:
    """36 字元：`-` 在 8/13/18/23、version nibble 在 14 是 `4`、
    variant nibble 在 19 屬 {8,9,a,b}（RFC 4122 §4.4），其餘 ASCII hex。"""
    for k in (8, 13, 18, 23):
        if s[at + k] != "-":
            return False
    if s[at + 14] != "4":
        return False
    if s[at + 19] not in ("8", "9", "a", "b", "A", "B"):
        return False
    return all(_is_ascii_hex(s[at + k]) for k in range(_UUID_LEN) if k not in (8, 13, 18, 23))


def _is_id_named_key(key: str) -> bool:
    """ASCII 小寫後的子字串比對。

    刻意不用 `str.lower()`：它是 Unicode 感知的，`İ`.lower() 會變成兩個
    code point，長度都可能改（M20 HTML 策略踩過的同一顆雷）。needle 全是
    ASCII，所以只把 A–Z 折下來就夠，也才和 Rust 的 `to_ascii_lowercase` 一致。
    """
    lowered = "".join(chr(ord(c) + 32) if "A" <= c <= "Z" else c for c in key)
    return any(needle in lowered for needle in _ID_FIELD_NEEDLES)


def _is_value_empty(value) -> bool:
    """空字串 / 空陣列 / 空物件 / null 一律視為「沒有值」——
    只是在 schema 裡『宣告』了 request_id 的 client 不該被指控。"""
    if value is None:
        return True
    if isinstance(value, (str, list, dict)):
        return len(value) == 0
    return False


def _describe(value) -> str:
    """把值渲染成**型別描述**，絕不含值本身（見模組 docstring 的 sample 政策）。

    長度以字元計（Rust 端用 `chars().count()`）—— 用 byte 會讓非 ASCII 值
    的描述在兩邊分岔。
    """
    if isinstance(value, str):
        return f"string[{len(value)}]"
    # bool 必須排在 int 之前 —— Python 的 bool 是 int 的子類。
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, list):
        return f"array[{len(value)}]"
    if isinstance(value, dict):
        return f"object[{len(value)}]"
    return "null"


# ─── 輸入收斂（與 serde_json 的判準對齊）───────────────────────────────


def _reject_constant(name: str):
    """`NaN` / `Infinity` / `-Infinity`：serde_json 一律 Err，這裡跟著拒收。

    拋 ValueError 讓 `scan_request` 既有的 except 接住 —— 而且要注意 Rust
    是**整包** parse 失敗，所以拒收的粒度也必須是整份文件，不是那個欄位。
    """
    raise ValueError(f"JSON 常數 {name} 不被接受（與 serde_json 對齊）")


def _has_lone_surrogate_escape(text: str) -> bool:
    """原始 JSON 文字裡有沒有落單的 surrogate 跳脫。

    Python 的 json 會把 `\\ud800` 解成一個無法編碼成 UTF-8 的 str，
    serde_json 則要求 high/low 成對、否則整包 Err。**成對的必須放行**
    （`\\ud83d\\ude00` 是合法的 emoji），所以不能無腦擋掉所有 surrogate ——
    守門要同時測「該擋的擋了」與「該過的還會過」。
    """
    i = 0
    n = len(text)
    while True:
        i = text.find("\\u", i)
        if i < 0:
            return False
        # 往回數連續反斜線；偶數個代表它們兩兩成對，這個 `u` 是普通字元。
        start = i
        while start > 0 and text[start - 1] == "\\":
            start -= 1
        if (i - start + 1) % 2 == 0:
            i += 2
            continue
        code = _hex4(text, i + 2)
        if code is None:
            i += 2
            continue
        if 0xDC00 <= code <= 0xDFFF:
            return True  # 落單的 low surrogate
        if 0xD800 <= code <= 0xDBFF:
            if text[i + 6 : i + 8] != "\\u":
                return True
            low = _hex4(text, i + 8)
            if low is None or not (0xDC00 <= low <= 0xDFFF):
                return True
            i += 12
            continue
        i += 6


def _hex4(text: str, at: int) -> int | None:
    chunk = text[at : at + 4]
    if len(chunk) < 4 or not all(_is_ascii_hex(c) for c in chunk):
        return None
    return int(chunk, 16)


def _nesting_depth(obj) -> int:
    """最大容器巢狀深度。**迭代實作** —— 用遞迴去量遞迴深度，量到一半就
    自己爆了，正是這個函式要防的事。"""
    depth = 0
    stack = [(obj, 1)]
    while stack:
        node, d = stack.pop()
        if d > depth:
            depth = d
        children = node.values() if isinstance(node, dict) else node
        for child in children:
            if isinstance(child, (dict, list)):
                stack.append((child, d + 1))
    return depth
