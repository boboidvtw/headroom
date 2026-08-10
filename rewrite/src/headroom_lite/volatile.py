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

刻意偏離解答本的三處（各有理由，不是漏做）
------------------------------------------
  a. **入口吃 bytes、自己 parse 一份副本**。工業版收 `&Value`（proxy 那層
     已經 parse 過，省一次 JSON 成本），非變性靠呼叫端的 debug_assert 與
     整合測試守。重建反過來：多付一次 parse，換「掃描器手上根本沒有呼叫端
     的物件」—— 非變性從『有測試守著』升級成『結構上不可能違反』。
  b. **sample 截斷用字元數不是 byte 數**。工業版切 80 bytes 並小心 UTF-8
     邊界；重建切 80 個字元，因為 Python 依 code point、Rust 依 byte，
     只有用字元上限兩邊才會吐出同一串 sample（parity 是重建的一等目標，
     工業版沒有這個約束）。上限仍然有界（<= 320 bytes）。
  c. **沒有 ApiKind 分歧**。工業版要同時走 Anthropic 與 OpenAI 兩種 body
     形狀；重建全程只有 `/v1/messages`，多一個 enum 只是憑空的分支。
     —— 承 READING-05 的教訓：把「這裡為什麼不需要」寫下來，和寫守門
     一樣有價值，否則下一個人會把它當成漏掉的。

policy（承襲工業版）
--------------------
  - **不用 regex**：每個 pattern 都是明寫的位元組位置檢查，意圖看得見。
  - **findings 上限 10**：客戶貼一份 CSV 進 system prompt 就能產出幾百條
     警告淹掉 log。前 1–3 條就是他會動手修的那幾條。
  - **sample 截斷**：撈一小片讓客戶定位得到，但絕不整包記客戶資料。
"""

from __future__ import annotations

import json
from dataclasses import dataclass

# kind 的字串表示是穩定介面（parity 報告與觀測線都吃它），別隨手改。
TIMESTAMP = "iso8601_timestamp"
UUID_V4 = "uuid_v4"
ID_FIELD = "id_field"

MAX_FINDINGS = 10
SAMPLE_MAX_CHARS = 80

# 慣例上「每請求唯一」的 JSON key 名。對 key 做 ASCII 小寫後的子字串比對
# —— `x_request_id`、`meta_session_id` 都認得。
_ID_FIELD_NEEDLES = ("request_id", "trace_id", "session_id", "correlation_id")

_ISO_LEN = 19  # `YYYY-MM-DDTHH:MM:SS`
_UUID_LEN = 36


@dataclass(frozen=True)
class VolatileFinding:
    """一筆發現。

    location 是 JSON-pointer 風格的路徑（`system[2].text`、
    `tools[0].input_schema.properties.session_id`），讓使用者把警告對回
    自己 request 裡的確切欄位。sample 是截斷過的節錄。
    """

    kind: str
    location: str
    sample: str


# ─── 入口 ──────────────────────────────────────────────────────────────


def scan_request(raw: bytes) -> list[VolatileFinding]:
    """掃描 request body bytes。永不修改任何東西、永不拋例外。

    壞輸入（非 JSON / 非 object）回空 list —— 與 M0 起的失敗模式契約一致：
    看不懂的東西就當作沒事，絕不因為觀測而影響請求。

    parse_float / parse_int = str：讓數字以**原始字面值**進來，對齊 Rust 的
    `arbitrary_precision`。否則 `1.10` 在 Python 會變 `1.1`，兩邊 sample 分岔
    （M15 JSON 策略踩過同一顆雷）。掃描器不重新序列化，所以這樣讀完全安全。
    """
    try:
        body = json.loads(raw, parse_float=str, parse_int=str)
    except (ValueError, UnicodeDecodeError):
        return []
    if not isinstance(body, dict):
        return []
    return detect_volatile_content(body)


def detect_volatile_content(body: dict) -> list[VolatileFinding]:
    """走訪 Anthropic `/v1/messages` 形狀的快取熱區。唯讀。

    走訪順序固定（system → messages → tools）且 dict 依插入順序 ——
    Rust 端開了 preserve_order，兩邊撞到上限時砍掉的是同一批。
    """
    out: list[VolatileFinding] = []

    system = body.get("system")
    if system is not None:
        _scan_content(system, "system", out)

    messages = body.get("messages")
    if isinstance(messages, list):
        # **最後一則不掃**（刻意偏離解答本的 E5，它掃全部 messages）。
        #
        # 快取前綴的邊界由 M3 的 `_place_breakpoints` 定義：標記 2 放在
        # `messages[-2]`，所以 `messages[-1]` 從來就不在前綴裡 —— 那是
        # live zone，它每輪都變、變了無害，也正是壓縮引擎接著要改寫的東西。
        #
        # 這不是潔癖。上限是全域的、走訪順序是 system → messages → tools，
        # 光一則塞滿時間戳的 tool_result 就能灌滿 10 筆，把 tools 裡真正
        # 該報的東西安靜擠掉 —— 噪音之外還會漏報。
        # （同 drift_detector「先分清哪些變化是預期的，才有辦法對非預期的
        #   變化發警報」；否則每次請求都在漂，警報等於雜訊。）
        for i, message in enumerate(messages[:-1]):
            if len(out) >= MAX_FINDINGS:
                return out
            if isinstance(message, dict) and "content" in message:
                _scan_content(message["content"], f"messages[{i}].content", out)

    tools = body.get("tools")
    if isinstance(tools, list):
        for i, tool in enumerate(tools):
            if len(out) >= MAX_FINDINGS:
                return out
            if not isinstance(tool, dict):
                continue
            description = tool.get("description")
            if isinstance(description, str):
                _scan_string(description, f"tools[{i}].description", out)
            if "input_schema" in tool:
                _scan_value(tool["input_schema"], f"tools[{i}].input_schema", out)

    return out


# ─── 走訪 ──────────────────────────────────────────────────────────────


def _scan_content(value, location: str, out: list[VolatileFinding]) -> None:
    """content 位置：可能是字串、可能是 block 陣列、也可能是 object。"""
    if len(out) >= MAX_FINDINGS:
        return
    if isinstance(value, str):
        _scan_string(value, location, out)
    elif isinstance(value, list):
        for i, item in enumerate(value):
            if len(out) >= MAX_FINDINGS:
                return
            _scan_value(item, f"{location}[{i}]", out)
    elif isinstance(value, dict):
        _scan_value(value, location, out)


def _scan_value(value, location: str, out: list[VolatileFinding]) -> None:
    """唯一會檢查 **key 名稱** 的走訪器：tool input_schema、巢狀 block
    都流經這裡。"""
    if len(out) >= MAX_FINDINGS:
        return
    if isinstance(value, str):
        _scan_string(value, location, out)
    elif isinstance(value, list):
        for i, item in enumerate(value):
            if len(out) >= MAX_FINDINGS:
                return
            _scan_value(item, f"{location}[{i}]", out)
    elif isinstance(value, dict):
        for key, sub in value.items():
            if len(out) >= MAX_FINDINGS:
                return
            if _is_id_named_key(key) and not _is_value_empty(sub):
                out.append(
                    VolatileFinding(
                        kind=ID_FIELD,
                        location=f"{location}.{key}",
                        sample=_truncate_sample(_value_to_sample(sub)),
                    )
                )
                if len(out) >= MAX_FINDINGS:
                    return
            _scan_value(sub, f"{location}.{key}", out)


def _scan_string(text: str, location: str, out: list[VolatileFinding]) -> None:
    """在一段字串裡找時間戳與 UUID v4。同一段字串裡多次命中各記一筆。

    這裡逐 **code point** 掃，Rust 端逐 **byte** 掃 —— 兩邊的索引不同，
    但兩個 pattern 都是純 ASCII，所以認出來的是同一個子字串、跳過的也是
    同一段（19 個 ASCII 字元 == 19 bytes）。而且我們只回報子字串本身、
    從不回報偏移量，findings 因此逐字對齊。（M15/M20 的 native-index 範式。）
    """
    n = len(text)
    i = 0
    while i < n:
        if len(out) >= MAX_FINDINGS:
            return
        # 先試 ISO-8601：視窗較短，字串剛好在 UUID 中間結束時比較不會漏。
        if i + _ISO_LEN <= n and _looks_like_iso8601(text, i):
            out.append(
                VolatileFinding(
                    kind=TIMESTAMP,
                    location=location,
                    sample=_truncate_sample(text[i : i + _ISO_LEN]),
                )
            )
            i += _ISO_LEN
            continue
        if i + _UUID_LEN <= n and _looks_like_uuid_v4(text, i):
            out.append(
                VolatileFinding(
                    kind=UUID_V4,
                    location=location,
                    sample=_truncate_sample(text[i : i + _UUID_LEN]),
                )
            )
            i += _UUID_LEN
            continue
        i += 1


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
    return all(
        _is_ascii_hex(s[at + k]) for k in range(_UUID_LEN) if k not in (8, 13, 18, 23)
    )


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


def _value_to_sample(value) -> str:
    """把 JSON 值渲染成短樣本。數字已經是原始字面值的字串（見 scan_request
    的 parse_float/parse_int），直接用即可。"""
    if isinstance(value, str):
        return value
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    # list / dict：用 compact JSON，反正下面還會截斷。
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def _truncate_sample(s: str) -> str:
    if len(s) <= SAMPLE_MAX_CHARS:
        return s
    return s[:SAMPLE_MAX_CHARS] + "…"
