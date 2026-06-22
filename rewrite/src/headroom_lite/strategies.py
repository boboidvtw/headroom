"""M11 — 壓縮策略 dispatcher（內容感知壓縮的骨架）。

從 M1 的「寫死頭尾截斷」進化為「content sniffing → 選策略」的可插拔架構。
每個策略是一對函式：
  - applies(text)：這段內容適不適用本策略？（content sniffing）
  - squeeze(text, store)：確定性壓縮；壓不動就回原文。

之後接 log / search / diff 等內容感知策略，只需多寫一個 Strategy 並
插進 STRATEGIES（排在 truncate catch-all 之前），dispatcher 不必改。

三條共用契約（與 M1 live_zone 完全一致，所有策略都得守）：
  1. 確定性：同輸入 bytes → 同輸出 bytes（無時間戳、無隨機、hash-keyed）。
     client 每輪重送完整歷史，第 N+1 輪重壓必須逐字節重現。
  2. CCR 可逆：產出有損輸出「之前」先 store.put 原文，標記內嵌 content_key
     —— content_key 是唯一真相來源，標記與 store 永遠對得上。
  3. 沒賺就不動：squeeze 回傳的若沒比原文短，呼叫端（_compress_block）負責
     fallback 回原 block。

store.put 時機是神聖 spec：只有「真的要產出有損輸出」那一刻才 put，門檻
沒過（例如行數太少）就絕不 put —— parity 逐字節依賴此時機（M6 移植教訓）。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable

from headroom_lite.ccr import content_key

# 確定性截斷參數：保留頭尾，中段以標記取代。
HEAD_LINES = 20
TAIL_LINES = 10


@dataclass(frozen=True)
class Strategy:
    """一個壓縮策略 = 內容嗅探 + 確定性壓縮。

    用具名函式對（而非繼承）表達，刻意對稱 Rust 側的 function-pointer 註冊表，
    讓兩語言的 dispatcher 結構逐行對照。
    """

    name: str
    applies: Callable[[str], bool]
    squeeze: Callable[..., str]


def _truncate_applies(text: str) -> bool:
    """catch-all：截斷對任何文字都適用，永遠回 True（殿後保底）。"""
    return True


def _truncate_squeeze(text: str, store=None) -> str:
    """確定性頭尾截斷：頭 + 標記 + 尾。承襲 M1 `_squeeze_text` 的 body 不變。

    標記內含「原文 SHA-256 前 16 碼 + 省略行數」：
      - hash 讓同一份原文永遠產生同一個標記（確定性的證明書），
        同時就是 CCR store 的取回 key。
      - 絕不放時間戳或隨機值 —— 那會讓第 N+1 輪重壓結果不同。
    """
    lines = text.splitlines()
    if len(lines) <= HEAD_LINES + TAIL_LINES:
        return text  # 行數太少，沒得壓 —— 不 put、原文回（神聖時機契約）

    if store is not None:
        store.put(text)  # 壓縮前先收好原文 —— 永不丟資料
    digest = content_key(text)
    omitted = len(lines) - HEAD_LINES - TAIL_LINES
    marker = f"[... headroom-lite squeezed {omitted} lines | sha256:{digest} ...]"
    return "\n".join([*lines[:HEAD_LINES], marker, *lines[-TAIL_LINES:]])


# truncate 是永遠適用的 catch-all，必須殿後（內容感知策略排它前面）。
TRUNCATE = Strategy("truncate", _truncate_applies, _truncate_squeeze)


# ── M12 — log 內容感知策略（第一片真正的「嗅探→壓」內容策略）──
#
# 與盲目頭尾截斷的差別：log 行可逐行依「嚴重度」分類。把噪音（TRACE/DEBUG/INFO）
# 丟掉、保留所有高嚴重度（WARN/ERROR/...）與其他行。為何更好？散落在「中段」的
# ERROR —— truncate 只留頭尾、會把它們連同中段一起丟掉；log 策略逐行嗅探，每個
# error 都留下。代價是位元壓縮率可能不如截斷，但保住的是訊號（內容感知的取捨：
# 寧可多留 error，也不盲砍）。噪音佔比不夠高就不認領，讓 truncate 兜底。

MIN_LOG_LINES = 6  # 太少行不值得當 log 處理
LOG_RATIO = 0.6  # 可分類行（含 level token）佔非空行比例下限
NOISE_RATIO = 0.3  # 可丟噪音行佔比下限 —— 低於此交給 truncate（避免吃掉全高嚴重度 log）

# 嚴重度 token：ASCII 大寫、以「整詞」比對（前後皆非 ASCII 英數）。
# WARNING 排在 WARN 前無妨：兩者都歸 keep，整詞比對也不會把 WARN 誤配進 WARNING。
_KEEP_TOKENS = (b"WARNING", b"WARN", b"ERROR", b"FATAL", b"CRITICAL")
_DROP_TOKENS = (b"TRACE", b"DEBUG", b"INFO")


def _is_ascii_alnum(b: int) -> bool:
    """ASCII 英數判斷 —— 刻意只認 ASCII，與 Rust `is_ascii_alphanumeric` 逐字節對齊
    （Python `str.isalnum()` 認 Unicode，會讓中文等被當英數而與 Rust 分岔）。"""
    return 0x30 <= b <= 0x39 or 0x41 <= b <= 0x5A or 0x61 <= b <= 0x7A


def _contains_word(line: bytes, token: bytes) -> bool:
    """token 是否以「整詞」出現在 line（前後皆字串邊界或非 ASCII 英數）。

    對 bytes 操作（非 str），與 Rust 端同走 byte 視角 —— 多位元組 UTF-8 字元的
    每個 byte 都 >127，兩語言一致視為詞邊界。
    """
    n = len(token)
    start = 0
    while True:
        i = line.find(token, start)
        if i == -1:
            return False
        before_ok = i == 0 or not _is_ascii_alnum(line[i - 1])
        after_ok = i + n == len(line) or not _is_ascii_alnum(line[i + n])
        if before_ok and after_ok:
            return True
        start = i + 1


def _severity(line: str) -> str:
    """分類一行：'keep'（高嚴重度，保留）/ 'drop'（噪音，可丟）/ 'other'（無 token，保留）。"""
    lb = line.encode("utf-8")
    if any(_contains_word(lb, t) for t in _KEEP_TOKENS):
        return "keep"
    if any(_contains_word(lb, t) for t in _DROP_TOKENS):
        return "drop"
    return "other"


def _log_applies(text: str) -> bool:
    """嗅探：夠多行像 log、且噪音佔比夠高（值得丟）才認領；否則讓 truncate 兜底。"""
    nonempty = [line for line in text.split("\n") if line.strip()]
    total = len(nonempty)
    if total < MIN_LOG_LINES:
        return False
    drop = classified = 0
    for line in nonempty:
        sev = _severity(line)
        if sev != "other":
            classified += 1
        if sev == "drop":
            drop += 1
    if drop == 0:
        return False
    return classified / total >= LOG_RATIO and drop / total >= NOISE_RATIO


def _log_squeeze(text: str, store=None) -> str:
    """丟掉噪音行（TRACE/DEBUG/INFO），保留高嚴重度與其他行，末尾附一行標記。

    標記內含「丟掉行數 + 原文 content_key」—— key 是 CCR store 的取回鑰匙，也是
    確定性證明書（同原文永遠同標記）。沒噪音可丟 → 原文回、絕不 put（神聖時機契約）。
    """
    lines = text.split("\n")
    severities = [_severity(line) for line in lines]
    dropped = sum(1 for s in severities if s == "drop")
    if dropped == 0:
        return text  # 沒得丟 → 原文回，不 put
    if store is not None:
        store.put(text)  # 產出有損輸出前先收存原文 —— 永不丟資料
    digest = content_key(text)
    kept = [line for line, sev in zip(lines, severities) if sev != "drop"]
    marker = f"[... headroom-lite dropped {dropped} log lines | sha256:{digest} ...]"
    return "\n".join([*kept, marker])


# log 是內容感知策略，排在 truncate catch-all 之前。
LOG = Strategy("log", _log_applies, _log_squeeze)


# ── M13 — diff 內容感知策略（第二片「嗅探→壓」內容策略）──
#
# 與盲目頭尾截斷的差別：unified/git diff 可逐行依「角色」分類。把未變更的
# context 行（` ` 空格開頭）丟掉、保留所有結構行：hunk header（`@@`）、檔頭
# （`diff`/`index`/`---`/`+++`）、與所有 `+`/`-` 變更行。為何更好？散落在大段
# context 中的零星變更 —— truncate 只留頭尾、會把中段的變更連同 context 一起丟；
# diff 策略逐行嗅探，每個變更與 hunk header 都留下（hunk header 已編碼行號範圍，
# 足以定位；CCR store 保有完整原文可逆取回）。噪音（context）佔比不夠高就不認領，
# 讓 truncate 兜底。標記格式與 Rust 版逐字相同 → parity。
#
# 與 log 對稱、刻意全用 ASCII byte 前綴比對（`startswith(" ")` / `startswith("@@")`），
# 不走 `str.strip()` —— 避開 Python `strip()` 認 unicode 空白、與 Rust `trim` 分岔的地雷。

MIN_DIFF_LINES = 6  # 太少行不值得當 diff 處理
DIFF_CONTEXT_RATIO = 0.3  # 可丟 context 行佔比下限 —— 低於此交給 truncate


def _diff_applies(text: str) -> bool:
    """嗅探：像 unified diff（有 hunk header）、且 context 佔比夠高才認領；否則讓 truncate 兜底。"""
    lines = text.split("\n")
    # hunk header（`@@ -a,b +c,d @@`）是 diff 的獨有信號 —— 沒有就不是 diff，避免誤判
    # markdown 的 `+`/`-` 條列。
    if not any(line.startswith("@@") for line in lines):
        return False
    total = len(lines)
    if total < MIN_DIFF_LINES:
        return False
    context = sum(1 for line in lines if line.startswith(" "))
    if context == 0:
        return False
    return context / total >= DIFF_CONTEXT_RATIO


def _diff_squeeze(text: str, store=None) -> str:
    """丟掉 context 行（` ` 空格開頭），保留 hunk header / 檔頭 / 所有 +/- 變更行，末尾附標記。

    標記內含「丟掉行數 + 原文 content_key」—— key 是 CCR store 的取回鑰匙，也是
    確定性證明書（同原文永遠同標記）。沒 context 可丟 → 原文回、絕不 put（神聖時機契約）。
    """
    lines = text.split("\n")
    dropped = sum(1 for line in lines if line.startswith(" "))
    if dropped == 0:
        return text  # 沒得丟 → 原文回，不 put
    if store is not None:
        store.put(text)  # 產出有損輸出前先收存原文 —— 永不丟資料
    digest = content_key(text)
    kept = [line for line in lines if not line.startswith(" ")]
    marker = f"[... headroom-lite dropped {dropped} diff context lines | sha256:{digest} ...]"
    return "\n".join([*kept, marker])


# diff 是內容感知策略，排在 truncate catch-all 之前；亦排在 log 之前 —— 帶 `@@` 的
# diff 結構比 log 嚴重度分類更該優先保留（既有 log 內容無 hunk header，互不干擾）。
DIFF = Strategy("diff", _diff_applies, _diff_squeeze)


# ── M14 — search 內容感知策略（第三片「嗅探→壓」內容策略）──
#
# 對象：grep/rg 的 `file:lineno:content` 輸出。噪音 = 同一檔案的大量重複命中；
# 訊號 = 命中分布在哪些檔、每檔的代表性前幾筆。壓法：每檔保留前 KEEP_PER_FILE 筆、
# 其餘丟掉（保序），末尾附標記。非 match 行（heading、空行、context 分隔）一律保留。
#
# ⚠️ parity 地雷（誠實記錄）：若只用「`:` 分隔、第二欄全數字」判 match line，會誤判 log
# 時間戳 `10:30:45`（正是 `欄:數字:欄`）→ search 反吃 log、污染 M12 行為。解法：要求
# file_key（首個 `:` 前）**含 `/`**。真實 `grep -rn pat .` 必帶路徑（`./src/foo.py:12:`）、
# 時間戳前綴（`2026-06-20T10`）無 `/` → 乾淨排除。純 byte 檢查（`"/" in s`），兩語言一致。
# 代價：cwd 下無 `/` 的單檔 grep（`foo.txt:12:`）不認領 → 落 truncate 兜底，可接受。
#
# 與 log/diff 對稱：嗅探只看「會丟多少」（drop flags），不另設玄學門檻；Rust squeeze 純
# 函式回 None、put 在呼叫端。確定性靠「保序逐行掃 + per-file 計數」，從不迭代 dict → 無
# 雜湊順序依賴，Py/Rs 逐字節一致。

MIN_SEARCH_LINES = 6  # 太少行不值得當 search 處理
SEARCH_DROP_RATIO = 0.3  # 可丟（超出每檔上限）行佔比下限 —— 低於此交給 truncate
KEEP_PER_FILE = 3  # 每檔保留的代表性命中筆數


def _match_line_key(line: str):
    """grep/rg match 行判斷：回 (是否 match, file_key)。

    形如 `file:lineno:content`，且 file_key 必須含 `/`（排除 log 時間戳誤判）、
    lineno 必須非空且全 ASCII 數字。非 match 行回 (False, "")。
    """
    i1 = line.find(":")
    if i1 <= 0:  # 無冒號，或 file_key 為空（行首即冒號）
        return False, ""
    file_key = line[:i1]
    if "/" not in file_key:  # 必須像路徑 —— 擋掉 `10:30:45` 這類時間戳
        return False, ""
    i2 = line.find(":", i1 + 1)
    if i2 == -1:
        return False, ""
    lineno = line[i1 + 1 : i2]
    # 刻意只認 ASCII 數字（與 Rust `is_ascii_digit` 逐字節對齊；不用 str.isdigit 認 unicode）
    if not lineno or not all(0x30 <= ord(c) <= 0x39 for c in lineno):
        return False, ""
    return True, file_key


def _search_drop_flags(lines: list[str]) -> list[bool]:
    """保序逐行掃：每個 file_key 計數，超過 KEEP_PER_FILE 的 match 行標記為「丟」。

    只用 dict 做計數查找、從不迭代 dict —— 結果僅依輸入順序，無雜湊順序依賴（parity）。
    """
    counts: dict[str, int] = {}
    flags: list[bool] = []
    for line in lines:
        is_match, key = _match_line_key(line)
        if not is_match:
            flags.append(False)
            continue
        counts[key] = counts.get(key, 0) + 1
        flags.append(counts[key] > KEEP_PER_FILE)
    return flags


def _search_applies(text: str) -> bool:
    """嗅探：夠多行、且超出每檔上限的可丟命中佔比夠高才認領；否則讓後手兜底。"""
    lines = text.split("\n")
    total = len(lines)
    if total < MIN_SEARCH_LINES:
        return False
    dropped = sum(_search_drop_flags(lines))
    if dropped == 0:
        return False
    return dropped / total >= SEARCH_DROP_RATIO


def _search_squeeze(text: str, store=None) -> str:
    """每檔保留前 KEEP_PER_FILE 筆命中、丟其餘，末尾附標記。

    沒超量可丟 → 原文回、絕不 put（神聖時機契約）。
    """
    lines = text.split("\n")
    flags = _search_drop_flags(lines)
    dropped = sum(flags)
    if dropped == 0:
        return text  # 沒得丟 → 原文回，不 put
    if store is not None:
        store.put(text)  # 產出有損輸出前先收存原文 —— 永不丟資料
    digest = content_key(text)
    kept = [line for line, drop in zip(lines, flags) if not drop]
    marker = f"[... headroom-lite dropped {dropped} search result lines | sha256:{digest} ...]"
    return "\n".join([*kept, marker])


# search 排在 diff 之後、log 之前：grep-over-logs 由 search 接管（貼合「跨檔看命中」意圖）；
# 既有 log 內容無 `/`+數字 match 行 → search 不認領，不回歸 M12 行為。
SEARCH = Strategy("search", _search_applies, _search_squeeze)


# ── M15 — json 內容感知策略（第四片「嗅探→壓」內容策略）──
#
# 對象：大型 JSON 文件（API 回應、jq 輸出等）。噪音 = 同質元素的大型 array；訊號 =
# 結構與頭尾代表性元素。壓法：找元素最多的 array，保前 JSON_HEAD + 後 JSON_TAIL 個元素、
# 中間塞一個 marker 字串元素（結果仍是合法 JSON array），array 外的 bytes 照抄。
#
# ⭐ parity 正解（本片最重要的設計）：**絕不重序列化任何值**。先前評估 JSON 的地雷是
# 「截斷後 json.dumps 把 `1.10` 正規化成 `1.1`，而 Rust arbitrary_precision 保留原樣 →
# 分岔」。解法不是硬扛 number encoder 對齊，而是改用 **byte-level 結構掃描**：被保留的
# 元素一律照抄原始 bytes 切片（數字/字串/巢狀物件原文不動），唯一新寫的是結構字元
# （`[` `,` `]`）與 marker —— 全是 ASCII 常數。如此 Python 與 Rust 不可能在值上分岔。
#
# 掃描器逐字元走訪、追蹤字串字面值（跳過字串內的括號/逗號）與巢狀深度，逐 array 記錄
# 元素 byte span。確定性：tie-break 取「元素最多；同票取 start 最小（源序最前）」，Py/Rs
# 一致（Python max / Rust max_by_key 在同票時行為相反，故顯式用『嚴格大於才替換』保最前）。
#
# 防誤判：要求 content 首個非空白字元是 `[`/`{`（純 ASCII 檢查）—— 只吃真 JSON 文件，
# 不碰含括號的 log/prose。收存點不對稱承襲 M11–M14：核心純函式、put 在呼叫端。

JSON_HEAD = 5  # 保留 array 開頭元素數
JSON_TAIL = 2  # 保留 array 結尾元素數
MIN_JSON_DROP = 4  # 至少要丟這麼多元素才值得壓（否則 marker 開銷可能蓋過收益）


def _starts_json(text: str) -> bool:
    """首個非 ASCII 空白字元是否為 `[`/`{`（判斷整段 content 是不是 JSON 文件）。"""
    for c in text:
        if c in " \t\n\r":
            continue
        return c in "[{"
    return False


def _scan_arrays(text: str) -> list[tuple[int, int, list[tuple[int, int]]]]:
    """單次線性掃描，回所有 JSON array 的 (start, end, elem_spans)。

    elem_spans = 各元素的 (start, end) char offset（含前後空白、不含分隔逗號）。正確處理
    巢狀（只在「目前最內層是 array」時才把逗號當元素分隔）與字串字面值（跳過字串內字元）。
    """
    arrays: list[tuple[int, int, list[tuple[int, int]]]] = []
    stack: list[dict] = []
    i, n = 0, len(text)
    in_string = escape = False
    while i < n:
        c = text[i]
        if in_string:
            if escape:
                escape = False
            elif c == "\\":
                escape = True
            elif c == '"':
                in_string = False
            i += 1
            continue
        if c == '"':
            in_string = True
        elif c in "[{":
            frame = {"kind": c, "start": i, "elements": [], "elem_start": i + 1 if c == "[" else None}
            stack.append(frame)
        elif c in "]}":
            frame = stack.pop() if stack else None
            if frame is not None and frame["kind"] == "[" and c == "]":
                s = frame["elem_start"]
                # 收最後一個元素；空 array（[]）或只有空白 → 不計為元素
                if frame["elements"] or text[s:i].strip() != "":
                    frame["elements"].append((s, i))
                arrays.append((frame["start"], i + 1, frame["elements"]))
        elif c == "," and stack and stack[-1]["kind"] == "[":
            frame = stack[-1]
            frame["elements"].append((frame["elem_start"], i))
            frame["elem_start"] = i + 1
        i += 1
    return arrays


def _json_squeeze_core(text: str) -> str | None:
    """純函式：找元素最多的 array 截斷成頭+marker+尾。壓不動回 None。"""
    if not _starts_json(text):
        return None
    arrays = _scan_arrays(text)
    # tie-break：元素最多；同票取 start 最小（嚴格大於才替換 → 保源序最前，Py/Rs 一致）
    best: tuple[int, int, list[tuple[int, int]]] | None = None
    for arr in arrays:
        if best is None or len(arr[2]) > len(best[2]):
            best = arr
    if best is None:
        return None
    start, end, elems = best
    dropped = len(elems) - JSON_HEAD - JSON_TAIL
    if dropped < MIN_JSON_DROP:
        return None
    head = [text[s:e] for s, e in elems[:JSON_HEAD]]
    tail = [text[s:e] for s, e in elems[-JSON_TAIL:]]
    digest = content_key(text)
    marker = f'"[... headroom-lite dropped {dropped} array elements | sha256:{digest} ...]"'
    new_array = "[" + ",".join([*head, marker, *tail]) + "]"
    return text[:start] + new_array + text[end:]


def _json_applies(text: str) -> bool:
    """嗅探：是 JSON 文件、且最大 array 元素夠多（可丟 ≥ MIN_JSON_DROP）才認領。"""
    return _json_squeeze_core(text) is not None


def _json_squeeze(text: str, store=None) -> str:
    """截斷最大 array；壓不動 → 原文回、絕不 put（神聖時機契約）。"""
    out = _json_squeeze_core(text)
    if out is None:
        return text
    if store is not None:
        store.put(text)  # 產出有損輸出前先收存原文 —— 永不丟資料
    return out


# json 排最前：applies 極專一（需首字元 `[`/`{` + 11+ 元素 array），不會誤搶 diff/search/log；
# 真 JSON array 的 tool_result 本就該由此壓。
JSON = Strategy("json", _json_applies, _json_squeeze)

# 策略註冊表：按優先序排列。json/diff/search/log 先嗅探，不命中才落到 truncate 兜底。
STRATEGIES: tuple[Strategy, ...] = (JSON, DIFF, SEARCH, LOG, TRUNCATE)


def squeeze_text(text: str, store=None, strategies: tuple[Strategy, ...] = STRATEGIES) -> str:
    """dispatcher：選第一個 applies 命中的策略來壓，命中即停。

    strategies 預設為模組級註冊表；測試可注入自訂順序驗證 dispatch 行為。
    無任何策略命中（理論上不會，truncate 是 catch-all）→ 防禦性回原文。
    """
    for strategy in strategies:
        if strategy.applies(text):
            return strategy.squeeze(text, store)
    return text
