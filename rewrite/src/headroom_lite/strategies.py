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


# ── M21 — 建置/測試輸出的進度行 ──
#
# 為何需要這條：severity token 表（ERROR/WARN/INFO…）是為「應用程式 runtime log」
# 設計的體裁，而 pytest 說 FAILED、cargo 說 `error[E0382]`、jest 用符號 —— 一個都不命中。
# 結果是 log 策略對建置/測試輸出整個不認領，落到盲目頭尾截斷，把中段的 FAILURES
# 區塊丟掉；正是 M12 的註解宣稱這支策略要避免的事（缺陷實錄見 READING-02）。
#
# 修法刻意**不**擴充 token 表：那是開放集合，補完 pytest 還有 cargo、jest、make，
# 而「另一種工具碰巧不長這樣」永遠有下一個。改用結構訊號 —— 進度行是一長串狀態符號，
# 這個形狀與工具的用詞無關。
#
# 判準要兩個條件同時成立，因為光看「連續長度」會誤判目錄的點狀填充
# （`Chapter 3 .......... 42` 可以有十幾個點）：
#   1. 存在一段長度 >= MIN_PROGRESS_RUN 的進度符號連續段，且
#   2. 該行以 `%]` 收尾（pytest 的百分比欄），或整行只由進度符號與空白組成。
#
# 全程 ASCII byte 視角，與 Rust 端逐字節對齊。

MIN_PROGRESS_RUN = 8  # 連續進度符號的長度下限；低於此視為散文的刪節號
_PROGRESS_GLYPHS = b".sxXFEP"  # pytest：pass/skip/xfail/xpass/fail/error/其他狀態


def _is_progress_line(line: str) -> bool:
    """結構性判斷：這行是不是建置/測試輸出的進度行。"""
    lb = line.encode("utf-8")
    run = best = 0
    for b in lb:
        if b in _PROGRESS_GLYPHS:
            run += 1
            best = max(best, run)
        else:
            run = 0
    if best < MIN_PROGRESS_RUN:
        return False
    stripped = lb.strip()
    if stripped.endswith(b"%]"):
        return True
    # 整行只有進度符號與空白 —— 無路徑前綴、無百分比的裸進度行。
    return all(b in _PROGRESS_GLYPHS or b in b" \t" for b in stripped)


def _severity(line: str) -> str:
    """分類一行：'keep'（高嚴重度，保留）/ 'drop'（噪音，可丟）/ 'other'（無 token，保留）。

    順序：keep token 優先（嚴重度勝過形狀）→ 進度行 → drop token → other。
    """
    lb = line.encode("utf-8")
    if any(_contains_word(lb, t) for t in _KEEP_TOKENS):
        return "keep"
    if _is_progress_line(line):
        return "drop"
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

# 行的身分（M24）：命中行 / 其附屬 context 行 / 都不是。空字串讓 truthiness 天然
# 對應「不認領」，與 Rust 的 Option<(LineKind, &str)> 對稱。
LINE_MATCH = "match"
LINE_CONTEXT = "context"
LINE_OTHER = ""


def _marker_paths(line: str, sep: str) -> list[str]:
    """列出行內所有「`sep` 型行號標記」`<sep>\\d+<sep>` 之前的 path 候選，由左而右。

    只認 ASCII 數字（與 Rust `is_ascii_digit` 逐字節對齊；不用 str.isdigit 認 unicode）。
    候選必須非空且含 `/` —— 後者擋掉 log 時間戳（`10:30:45` 的 `10`、`2026-06-20` 的
    `2026` 都沒有 `/`）。

    **回傳全部候選、而不是碰到第一個就定案**，是 review 回合修掉的一條 HIGH：原本
    「取最早候選，失敗就 return」讓 `2026-06-20 ./src/foo.py:42:hit` 這種帶時間戳前綴的
    grep 輸出整行認不得（`2026` 無 `/` 就放棄了，右邊真正的 `:42:` 從沒被看到）。
    """
    out: list[str] = []
    n = len(line)
    i = 0
    while i < n:
        if line[i] == sep:
            j = i + 1
            while j < n and 0x30 <= ord(line[j]) <= 0x39:
                j += 1
            if j > i + 1 and j < n and (line[j] == ":" or line[j] == "-"):
                path = line[:i]
                if path and "/" in path:
                    out.append(path)
        i += 1
    return out


def _match_line_key(line: str):
    """單行的**命中行**判斷：回 (LINE_MATCH, file_key) 或 (LINE_OTHER, "")。

    命中行是 `:` 型標記（`file:lineno:content`）。context 行要靠整段的檔名白名單才能
    判定，見 `_classify_lines` —— 單獨一行無法可靠地分辨
    `./src/step-2-runner.py-41-ctx` 的哪個 `-` 才是標記。
    """
    for path in _marker_paths(line, ":"):
        return LINE_MATCH, path
    return LINE_OTHER, ""


def _classify_lines(lines: list[str]) -> list[tuple[str, str]]:
    """兩趟分類：先認命中行建立檔名白名單，再用白名單認 context 行。

    **為何是兩趟（review 回合的核心修正）**：原本「取行內最早的 `<sep>\\d+<sep>` 標記」
    在檔名含 `-數字-` 時整個垮掉 —— `./src/step-2-runner.py:42:hit` 的 `-2-` 比真標記
    `:42:` 更早，path 被切成 `./src/step`。而且傷害不是「這行判錯」而是**整段 SEARCH
    自己關掉**（命中行認不得 → 可丟數為 0 → 比率閘門不認領 → 落盲目截斷），正是 M24
    要修的那個病自己重演。這種檔名一點都不罕見：migration、step、part、版本號都是。

    白名單的作法：命中行的 `:` 型標記歧義小（路徑不含 `:`，Windows 路徑已被 `/` 擋在
    門外），先掃出所有命中行的 file_key；context 行再從自己的 `-` 型候選裡挑**出現在
    白名單中**的那一個。於是 `-2-`（→`./src/step`）被排除、`-41-`（→`./src/step-2-runner.py`）
    中選 —— 不是猜哪個候選才對，而是問「這個 path 是某個命中行認過的檔案嗎」。

    **這條白名單同時換掉了原本的空白黑名單**：舊防線「`-` 型的 path 不得含 ASCII 空白」
    被 U+3000 全形空白直接繞過（而這個里程碑的主題正是 CJK 路徑）。與其列舉更多空白
    字元 —— 那是開放集合 —— 不如換掉「列舉」這個作法本身：不在白名單裡的 path 一律不是
    context 行，`/usr/src/app.py<U+3000>2026-06-20 boom` 因此自然出局。

    白名單只做查找、從不迭代 → 無雜湊順序依賴（parity）。

    已知限制（都落回 truncate 兜底或保守保留，不會產出錯輸出）：
      - context 行的**內容**若含 `:數字:`（例如程式碼裡的時間戳字面值），該行會被當成
        命中行。後果是它被保守地保留而非跟隨 owner，不會誤丟資料。這是拿它換掉上面那個
        「整段關掉」的 CRITICAL —— 兩者不可兼得時，選傷害小的。
      - Windows 反斜線路徑（`C:\\Users\\me\\foo.py:42:hit`）不認領：`/` 檢查要求正斜線。
        自 M14 起如此，非 M24 引入。
      - `rg --heading` / `--no-filename` 的無路徑輸出（`42:content`）不認領，同樣自 M14 起。
    """
    known: dict[str, int] = {}
    parsed: list[tuple[str, str]] = []
    for line in lines:
        kind, key = _match_line_key(line)
        parsed.append((kind, key))
        if kind == LINE_MATCH:
            known[key] = 1

    for i, (kind, _) in enumerate(parsed):
        if kind != LINE_OTHER:
            continue
        for path in _marker_paths(lines[i], "-"):
            if path in known:  # 只查找、不迭代 → 無雜湊順序依賴
                parsed[i] = (LINE_CONTEXT, path)
                break
    return parsed


def _search_drop_flags(lines: list[str]) -> list[bool]:
    """保序逐行掃：每個 file_key 計數，超過 KEEP_PER_FILE 的**命中**標記為「丟」。

    M24 的歸屬規則：context 行不是獨立命中（否則 `rg -C 1` 下每檔留的 3 行會是
    context/match/context，真命中只剩 1 筆），而是**跟隨距離最近的同檔命中**、平手取前者。
    rg 的排版下這正好把 `after` 歸前一個命中、`before` 歸後一個命中，於是保留的命中連同
    context 一起保、丟掉的一起丟（不留孤兒 context）。

    只用 dict 做查找、從不迭代 dict；歸屬用純索引數學（前向/後向各一趟）——
    結果僅依輸入順序，無雜湊順序依賴（parity）。

    **前向/後向表是 per-key 的（review 回合修掉的一條 HIGH）**：原本只追一個「全域最近
    命中」再事後篩同檔，於是**別的檔案的命中插在中間就把同檔命中擋掉了** —— 歸屬落到
    「找不到 owner → 保留」，被丟的命中留下孤兒 context，違反這支函式自己宣稱的不變量。
    單次 `rg -C` 的輸出同檔連續、踩不到；但 headroom 壓的是 agent 串接起來的 tool_result，
    多次搜尋結果貼在一起是正常情境，而測試的產生器結構上從不產生交錯輸入。
    """
    parsed = _classify_lines(lines)
    n = len(lines)

    # 命中行的去留：每個 file_key 計數，超過上限即丟。
    counts: dict[str, int] = {}
    flags: list[bool] = [False] * n
    for i, (kind, key) in enumerate(parsed):
        if kind == LINE_MATCH:
            counts[key] = counts.get(key, 0) + 1
            flags[i] = counts[key] > KEEP_PER_FILE

    # 前向一趟：每個位置「同一個 key 最近的前一個命中」的 index（-1 = 沒有）。
    prev_idx: list[int] = [-1] * n
    last_by_key: dict[str, int] = {}
    for i, (kind, key) in enumerate(parsed):
        if kind == LINE_CONTEXT:
            prev_idx[i] = last_by_key.get(key, -1)
        if kind == LINE_MATCH:
            last_by_key[key] = i

    # 後向一趟：每個位置「同一個 key 最近的後一個命中」的 index（-1 = 沒有）。
    next_idx: list[int] = [-1] * n
    last_by_key = {}
    for i in range(n - 1, -1, -1):
        kind, key = parsed[i]
        if kind == LINE_CONTEXT:
            next_idx[i] = last_by_key.get(key, -1)
        if kind == LINE_MATCH:
            last_by_key[key] = i

    # context 行歸屬：距離小者勝，平手取前者；無同檔命中則保留。
    for i, (kind, key) in enumerate(parsed):
        if kind != LINE_CONTEXT:
            continue
        before_ok = prev_idx[i] >= 0
        after_ok = next_idx[i] >= 0
        if before_ok and after_ok:
            owner = prev_idx[i] if (i - prev_idx[i]) <= (next_idx[i] - i) else next_idx[i]
        elif before_ok:
            owner = prev_idx[i]
        elif after_ok:
            owner = next_idx[i]
        else:
            continue  # 找不到同檔命中 → 保留（保守）
        flags[i] = flags[owner]
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


# ── M22 — 罕見即資訊（第一個「按資訊選擇」的判準）──
#
# 頭 5 尾 2 的選擇依據是「排在第幾個」。100 筆健檢結果裡 3 筆 timeout 埋在中段，
# 壓縮率 92% 而三筆全滅、留下七筆一樣的 ok —— 模型會得出「一切正常」。輸出看起來
# 完全合理、指標還很漂亮，這正是它比 M21 那個缺陷更危險的地方（見 READING-03）。
#
# 判準取自 smart_crusher 的 detect_rare_status_values，且用的是它**修好 Bug #3 之後**
# 的版本：原版 `if not (2 <= len(unique_values) <= 10): continue` 會讓「保留罕見錯誤」
# 在錯誤種類一多時自己關掉 —— 而那正是最需要它的時候。改用 Pareto 檢查：
#
#   1. 相異值數在 [2, RARE_MAX_CARDINALITY] 之內（超過幾乎確定是 ID 或自由文字欄）
#   2. 值頻率降冪排序，找最小的 K 使 top-K 覆蓋 >= 80% 的項目
#   3. 若 K <= RARE_MAX_K，其餘的值即「罕見」，含有它們的元素進必留集合
#
# 三種分布都要對，包含**不該觸發**的那一種：低基數+主宰值（95 ok + 5 錯誤）→ 觸發；
# 雙峰（60 info + 25 warn + 15 種罕見錯誤）→ 觸發，這是舊版整個漏掉的；
# 均勻分布（每個值各出現一兩次）→ K 永遠達不到 80%，正確判定為非類別欄、不觸發。
#
# parity：80% 門檻用整數運算（cum * 100 >= total * 80）避免浮點分岔；鍵與同頻值都
# 顯式排序，絕不依賴 dict 迭代順序。

RARE_MAX_CARDINALITY = 50  # 相異值數上限；超過視為 ID/自由文字欄，非狀態列舉
RARE_COVERAGE_PCT = 80  # top-K 需覆蓋的項目佔比（整數百分比，避免浮點）
RARE_MAX_K = 5  # 覆蓋門檻所需的 K 上限；超過代表分布太平、不是類別欄
# 罕見保留的上限與判準同源：Pareto 已保證「單一欄位」的罕見值 <= 20%，但多個類別欄
# 各自標記的集合聯集起來可能超過，所以對聯集再套一次同樣的 20%。刻意不用絕對值上限
# （原本寫 10）—— 那會在罕見值剛好 15 個時安靜地丟掉 5 個最有資訊量的元素，
# 正是這個策略要避免的事。
RARE_MAX_KEEP_PCT = 20


def _json_string_pairs(elem: str) -> list[tuple[str, str]]:
    """從一個元素的原文抽出所有 `"key": "value"` 字串對（value 非字串者略過）。

    刻意不解析 JSON —— 本策略全程只做括號/字串掃描（承襲 _scan_arrays）。
    遇到 `,{}[]` 就把待配的 key 丟掉：那代表 value 是數字/bool/null/巢狀，不是字串。
    """
    pairs: list[tuple[str, str]] = []
    pending: str | None = None
    i, n = 0, len(elem)
    while i < n:
        c = elem[i]
        if c == '"':
            j, esc = i + 1, False
            while j < n:
                d = elem[j]
                if esc:
                    esc = False
                elif d == "\\":
                    esc = True
                elif d == '"':
                    break
                j += 1
            lit = elem[i + 1 : j]
            i = j + 1
            k = i
            while k < n and elem[k] in " \t\n\r":
                k += 1
            if k < n and elem[k] == ":":
                pending = lit
                i = k + 1
            elif pending is not None:
                pairs.append((pending, lit))
                pending = None
            continue
        if c in ",{}[]":
            pending = None
        i += 1
    return pairs


def _rare_value_indices(elem_texts: list[str]) -> list[int]:
    """回傳「帶有罕見類別值」的元素索引（升冪、已去重）。"""
    by_key: dict[str, dict[str, list[int]]] = {}
    for idx, t in enumerate(elem_texts):
        for key, val in _json_string_pairs(t):
            by_key.setdefault(key, {}).setdefault(val, []).append(idx)

    rare: set[int] = set()
    for key in sorted(by_key):  # 確定性：鍵排序，不依賴 dict 迭代順序
        values = by_key[key]
        if not (2 <= len(values) <= RARE_MAX_CARDINALITY):
            continue
        total = sum(len(v) for v in values.values())
        # 頻率降冪；同頻以值字串升冪 → 兩語言排序結果一致
        ordered = sorted(values.items(), key=lambda kv: (-len(kv[1]), kv[0]))
        cum = k_needed = 0
        for rank, (_, idxs) in enumerate(ordered, 1):
            cum += len(idxs)
            if cum * 100 >= total * RARE_COVERAGE_PCT:
                k_needed = rank
                break
        if k_needed == 0 or k_needed > RARE_MAX_K:
            continue
        for _, idxs in ordered[k_needed:]:
            rare.update(idxs)
    return sorted(rare)


def _json_squeeze_core(text: str) -> str | None:
    """純函式：找元素最多的 array，保留頭+尾+罕見值元素，其餘丟棄。壓不動回 None。"""
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
    elem_texts = [text[s:e] for s, e in elems]
    total = len(elems)

    keep = set(range(min(JSON_HEAD, total))) | set(range(max(0, total - JSON_TAIL), total))
    # 罕見元素只從「原本會被丟掉」的那些裡挑；聯集上限為總數的 RARE_MAX_KEEP_PCT %
    rare_cap = total * RARE_MAX_KEEP_PCT // 100
    for idx in [i for i in _rare_value_indices(elem_texts) if i not in keep][:rare_cap]:
        keep.add(idx)

    dropped = total - len(keep)
    if dropped < MIN_JSON_DROP:
        return None

    digest = content_key(text)
    parts: list[str] = []
    i = 0
    while i < total:
        if i in keep:
            parts.append(elem_texts[i])
            i += 1
            continue
        j = i
        while j < total and j not in keep:
            j += 1
        # 每一段連續丟棄各插一個 marker；無罕見元素時只有一段 → 與 M22 前逐字相同
        parts.append(f'"[... headroom-lite dropped {j - i} array elements | sha256:{digest} ...]"')
        i = j
    return text[:start] + "[" + ",".join(parts) + "]" + text[end:]


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


# ── M16 — stack trace 內容感知策略（第五片「嗅探→壓」內容策略）──
#
# 對象：遞迴爆炸 / 深框架的 stack trace（Python `File "..."`、Java/JS `at ...(`）。噪音 =
# 中段大量重複或框架內部 frame；訊號 = 頂層 frame（錯在哪）、底層 frame（根因/進入點）、
# 與所有非 frame 行（`Traceback` 標頭、最終 `XxxError: msg`、chained-exception 分隔線）。
# 壓法：把 trace 切成 frame，保前 STACK_KEEP_HEAD + 後 STACK_KEEP_TAIL 個 frame、丟中段
# frame（連同其續行），**所有非 frame 行一律保留**，丟棄處塞一個 marker。
#
# 與盲目頭尾截斷的差別：truncate 以「行」為單位、會切半個 frame（把 `File "..."` 與其
# 程式碼續行拆開），且若 trace 尾端有多行非 frame 訊息，truncate 的 tail 視窗可能擠掉最關鍵
# 的 `XxxError: msg`。stack 策略以 frame 為邊界、永不切半、且非 frame 訊號行恆保留。
#
# 與 log/diff/search 對稱、全用 byte 級判別（skip 0x20/0x09 前綴後比對 `File "` / `at `），
# 不走 `str.strip()` —— 避開 Python unicode 空白與 Rust `trim` 分岔。frame 切段與丟棄純靠
# index 數學（保序逐行掃 + frame 區段），無雜湊順序依賴 → Py/Rs 逐字節一致。
#
# 註冊排在 LOG 之後、TRUNCATE 之前：既有 log/diff/search/json fixture 由前面策略先接走，
# stack 只接「沒有其他策略認領、否則會落盲截斷」的純 stack trace → 零回歸。

MIN_STACK_LINES = 8  # 太少行不值得當 stack trace 處理
MIN_STACK_FRAMES = 10  # frame 數下限（HEAD3+TAIL3+至少丟 4）
STACK_KEEP_HEAD = 3  # 保留頂層 frame 數
STACK_KEEP_TAIL = 3  # 保留底層 frame 數
STACK_DROP_RATIO = 0.3  # 可丟行佔比下限 —— 低於此交給 truncate


def _strip_ascii_ws(line: str) -> str:
    """只去除前綴的 ASCII 空白（0x20 空格 / 0x09 tab）—— 與 Rust 逐字節對齊，
    不用 str.lstrip()（認 unicode 空白，會與 Rust 分岔）。"""
    i = 0
    while i < len(line) and line[i] in " \t":
        i += 1
    return line[i:]


def _is_frame_header(line: str) -> bool:
    """frame 標頭判斷：去 ASCII 前綴空白後，像 Python `File "..."` 或 Java/JS `at ...(...)`。

    `at ` 額外要求該行含 `(` —— 真 frame 帶 `(File:line)`，藉此擋掉 "at the store" 這類 prose。
    """
    s = _strip_ascii_ws(line)
    if s.startswith('File "'):
        return True
    if s.startswith("at ") and "(" in s:
        return True
    return False


def _is_continuation(line: str) -> bool:
    """frame 續行判斷：以 ASCII 空白（空格/tab）起頭的非空行（如 Python frame 下的程式碼行）。"""
    return len(line) > 0 and line[0] in " \t"


def _segment_frames(lines: list[str]) -> list[tuple[int, int]]:
    """把行序列切成 frame 區段，回各 frame 的 (start, end_exclusive) 行索引範圍。

    frame = 一個標頭行 + 其後的續行（縮排、非標頭），直到下一個標頭行或非續行為止。
    非 frame 行（標頭前的 preamble、最終錯誤訊息、chained-exception 分隔）不納入任何 frame。
    """
    frames: list[tuple[int, int]] = []
    i, n = 0, len(lines)
    while i < n:
        if _is_frame_header(lines[i]):
            start = i
            i += 1
            while i < n and not _is_frame_header(lines[i]) and _is_continuation(lines[i]):
                i += 1
            frames.append((start, i))
        else:
            i += 1
    return frames


def _stack_dropped_lines(lines: list[str]) -> set[int]:
    """回「應丟棄」的行索引集合：中段 frame（保前 HEAD + 後 TAIL 個 frame）的所有行。

    frame 數不足 → 空集合（不丟）。純 index 數學、保序 → 確定性、parity 友善。
    """
    frames = _segment_frames(lines)
    if len(frames) < MIN_STACK_FRAMES:
        return set()
    drop: set[int] = set()
    for fi in range(STACK_KEEP_HEAD, len(frames) - STACK_KEEP_TAIL):
        start, end = frames[fi]
        drop.update(range(start, end))
    return drop


def _stacktrace_applies(text: str) -> bool:
    """嗅探：夠多行、frame 數足、且中段可丟行佔比夠高才認領；否則讓 truncate 兜底。"""
    lines = text.split("\n")
    if len(lines) < MIN_STACK_LINES:
        return False
    drop = _stack_dropped_lines(lines)
    if not drop:
        return False
    return len(drop) / len(lines) >= STACK_DROP_RATIO


def _stacktrace_squeeze(text: str, store=None) -> str:
    """保前 HEAD + 後 TAIL 個 frame、丟中段 frame，非 frame 行全留，丟棄處塞單一 marker。

    沒可丟 frame → 原文回、絕不 put（神聖時機契約）。marker 含「丟掉 frame 數 + 原文
    content_key」—— key 是 CCR 取回鑰匙，也是確定性證明書。
    """
    lines = text.split("\n")
    drop = _stack_dropped_lines(lines)
    if not drop:
        return text  # 沒得丟 → 原文回，不 put
    frames = _segment_frames(lines)
    dropped_frames = len(frames) - STACK_KEEP_HEAD - STACK_KEEP_TAIL
    if store is not None:
        store.put(text)  # 產出有損輸出前先收存原文 —— 永不丟資料
    digest = content_key(text)
    marker = f"[... headroom-lite dropped {dropped_frames} stack frames | sha256:{digest} ...]"
    out: list[str] = []
    marker_emitted = False
    for idx, line in enumerate(lines):
        if idx in drop:
            if not marker_emitted:
                out.append(marker)  # 中段第一個丟棄處塞一次 marker，其餘丟棄行省略
                marker_emitted = True
            continue
        out.append(line)
    return "\n".join(out)


# stacktrace 排在 log 之後、truncate 之前：純 stack trace（無 INFO/DEBUG 噪音）不被 log
# 認領 → 落到此；既有 log/diff/search/json fixture 已由前面策略接走，stack 不回歸它們。
STACKTRACE = Strategy("stacktrace", _stacktrace_applies, _stacktrace_squeeze)


# ── M17 — CSV/表格 內容感知策略（第六片「嗅探→壓」內容策略）──
#
# 對象：CSV/TSV 等表格輸出（DB 查詢結果、`column` 對齊匯出、資料表 dump）。噪音 = 大量
# 同構資料列；訊號 = 表頭（欄名）+ 頭尾代表性資料列。壓法：保表頭 + 前 CSV_KEEP_HEAD
# + 後 CSV_KEEP_TAIL 列，中段以單一 marker 取代（CCR store 保原文可逆）。
#
# 與盲目頭尾截斷的差別：truncate 以「行」為單位、不理解「表頭=訊號」的語意——若資料列
# 多到把表頭擠出 HEAD_LINES 視窗（如前綴有 SQL/說明文字），欄名就此遺失，剩下的數字列
# 全無法判讀。CSV 策略明確把表頭釘在輸出第一行、再配頭尾代表列，丟的純是中段同構列。
#
# 嗅探（保守、強訊號防誤判）：去掉單一尾端換行後，**每一非空行**都以同一 delimiter
# （`,` 優先、再 `\t`）出現「相同次數且 ≥1」才認領 —— 散文不可能每行逗號數一致，藉此
# 擋掉誤判。含內部空行 → 不認領（非乾淨表格）。引號內逗號會破壞「每行同數」而自動落
# truncate 兜底（保守，可接受）。delimiter 計數純 ASCII byte（`,`=0x2C / `\t`=0x09 皆非
# UTF-8 續位元組，byte 數 == 字元數）→ Py/Rs 逐字節一致。
#
# 收存點對稱 M15：核心純函式回 Option、put 在呼叫端（_csv_squeeze）。

MIN_CSV_LINES = 8  # 太少行不值得當表格處理（cheap floor；真正門檻是 MIN_CSV_DROP）
CSV_KEEP_HEAD = 3  # 表頭後保留的前幾筆資料列
CSV_KEEP_TAIL = 2  # 保留的末幾筆資料列
MIN_CSV_DROP = 4  # 至少要丟這麼多列才值得壓（否則 marker 開銷蓋過收益）


def _csv_rows(text: str) -> list[str] | None:
    """判斷是否為乾淨表格：回 clean 行序列（含表頭）或 None。

    條件：去單一尾端換行後行數 >= MIN_CSV_LINES、無內部空行、且存在某 delimiter
    （`,` 優先、再 `\t`）讓「每一行」出現次數相同且 >= 1。delimiter 計數純 ASCII byte
    （str.count 對單 ASCII 字元 == byte 計數，與 Rust 逐字節對齊）。
    """
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines = lines[:-1]  # 容忍單一尾端換行
    if len(lines) < MIN_CSV_LINES:
        return None
    if any(line == "" for line in lines):
        return None  # 內部空行 → 非乾淨表格，落 truncate 兜底
    for delim in (",", "\t"):
        c0 = lines[0].count(delim)
        if c0 >= 1 and all(line.count(delim) == c0 for line in lines):
            return lines
    return None


def _csv_squeeze_core(text: str) -> str | None:
    """純函式：保表頭 + 頭尾資料列、中段塞 marker。壓不動回 None。"""
    lines = _csv_rows(text)
    if lines is None:
        return None
    dropped = (len(lines) - 1) - CSV_KEEP_HEAD - CSV_KEEP_TAIL
    if dropped < MIN_CSV_DROP:
        return None
    digest = content_key(text)
    header = lines[0]
    head = lines[1 : 1 + CSV_KEEP_HEAD]
    tail = lines[len(lines) - CSV_KEEP_TAIL :]
    marker = f"[... headroom-lite dropped {dropped} table rows | sha256:{digest} ...]"
    return "\n".join([header, *head, marker, *tail])


def _csv_applies(text: str) -> bool:
    """嗅探：是乾淨表格、且中段可丟列數夠多（>= MIN_CSV_DROP）才認領。"""
    return _csv_squeeze_core(text) is not None


def _csv_squeeze(text: str, store=None) -> str:
    """保表頭 + 頭尾列；壓不動 → 原文回、絕不 put（神聖時機契約）。"""
    out = _csv_squeeze_core(text)
    if out is None:
        return text
    if store is not None:
        store.put(text)  # 產出有損輸出前先收存原文 —— 永不丟資料
    return out


# csv 排在 stacktrace 之後、truncate 之前：表格無 JSON/diff/search/log/frame 結構 → 不被
# 前面策略認領、落到此；既有 fixture 已由前面策略接走，csv 只接純表格 → 零回歸。
CSV = Strategy("csv", _csv_applies, _csv_squeeze)


# ── M18 — Markdown table 內容感知策略（第七片「嗅探→壓」內容策略）──
#
# 對象：GitHub-flavored markdown 表格（LLM 輸出、文件、README 裡極常見）。噪音 = 大量同構
# 資料列；訊號 = 表頭（欄名）+ **分隔列 `|---|---|`**（定義欄位對齊、是合法 markdown 表格的
# 必要結構）+ 頭尾代表性資料列。壓法：保表頭 + 分隔列 + 前 MD_KEEP_HEAD + 後 MD_KEEP_TAIL
# 列，中段以單一 marker 取代（CCR store 保原文可逆）。
#
# 與 CSV（M17）的差別（不只是換 delimiter）：markdown 表格多了一條「分隔列」必須釘住保留
# —— truncate 以行為單位、會把表頭與分隔列一起擠出視窗；本策略明確把兩者釘在輸出最前。
#
# 嗅探（保守、強訊號防誤判）：去單一尾端換行後行數 >= MIN_MD_LINES、無內部空行、**每一行**
# 都含相同數量（>= 1）的 `|`（散文不可能每行 pipe 數一致），且**第二行是合法分隔列**（只由
# `|` `:` `-` 空白組成且至少一個 `-`）。分隔列是與 CSV/散文的關鍵鑑別子 —— CSV 資料無 pipe、
# 散文無「每行同 pipe 數 + 分隔列」。pipe/分隔列計數純 ASCII byte（`|`=0x7C / `-`=0x2D /
# `:`=0x3A 皆非 UTF-8 續位元組）→ 與 Python str.count 逐字節一致、Py/Rs parity。
#
# 收存點對稱 M15/M17：核心純函式回 Option、put 在呼叫端（_md_squeeze）。

MIN_MD_LINES = 8  # 太少行不值得當表格處理（cheap floor；真正門檻是 MIN_MD_DROP）
MD_KEEP_HEAD = 3  # 表頭+分隔列後保留的前幾筆資料列
MD_KEEP_TAIL = 2  # 保留的末幾筆資料列
MIN_MD_DROP = 4  # 至少要丟這麼多列才值得壓（否則 marker 開銷蓋過收益）


def _is_md_separator(line: str) -> bool:
    """markdown 表格分隔列判斷：只由 `|` `:` `-` 空白組成、且至少含一個 `-`。

    純 ASCII byte 檢查（非 ASCII 字元的 byte 皆 >127、不在允許集 → 自動排除），與 Rust 逐字節對齊。
    """
    bs = line.encode("utf-8")
    has_dash = 0x2D in bs
    return has_dash and all(b in (0x7C, 0x3A, 0x2D, 0x20) for b in bs)


def _md_rows(text: str) -> list[str] | None:
    """判斷是否為乾淨 markdown 表格：回 clean 行序列（含表頭、分隔列）或 None。

    條件：去單一尾端換行後行數 >= MIN_MD_LINES、無內部空行、每行 `|` 數相同且 >= 1、
    第二行是合法分隔列。`|` 計數純 ASCII byte（str.count 對單 ASCII 字元 == byte 計數）。
    """
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines = lines[:-1]  # 容忍單一尾端換行
    if len(lines) < MIN_MD_LINES:
        return None
    if any(line == "" for line in lines):
        return None  # 內部空行 → 非乾淨表格，落 truncate 兜底
    c0 = lines[0].count("|")
    if c0 < 1 or any(line.count("|") != c0 for line in lines):
        return None  # 每行 pipe 數須一致且 >= 1
    if not _is_md_separator(lines[1]):
        return None  # 第二行須為分隔列 —— 與 CSV/散文的關鍵鑑別子
    return lines


def _md_squeeze_core(text: str) -> str | None:
    """純函式：保表頭 + 分隔列 + 頭尾資料列、中段塞 marker。壓不動回 None。"""
    lines = _md_rows(text)
    if lines is None:
        return None
    data_rows = len(lines) - 2  # 扣除表頭 + 分隔列
    if data_rows < MD_KEEP_HEAD + MD_KEEP_TAIL + MIN_MD_DROP:
        return None
    dropped = data_rows - MD_KEEP_HEAD - MD_KEEP_TAIL
    digest = content_key(text)
    header = lines[0]
    sep = lines[1]
    head = lines[2 : 2 + MD_KEEP_HEAD]
    tail = lines[len(lines) - MD_KEEP_TAIL :]
    marker = f"[... headroom-lite dropped {dropped} markdown table rows | sha256:{digest} ...]"
    return "\n".join([header, sep, *head, marker, *tail])


def _md_applies(text: str) -> bool:
    """嗅探：是乾淨 markdown 表格、且中段可丟列數夠多（>= MIN_MD_DROP）才認領。"""
    return _md_squeeze_core(text) is not None


def _md_squeeze(text: str, store=None) -> str:
    """保表頭 + 分隔列 + 頭尾列；壓不動 → 原文回、絕不 put（神聖時機契約）。"""
    out = _md_squeeze_core(text)
    if out is None:
        return text
    if store is not None:
        store.put(text)  # 產出有損輸出前先收存原文 —— 永不丟資料
    return out


# markdown 排在 stacktrace 之後、csv 之前：markdown 表格（pipe + 分隔列）比逗號 CSV 更專一，
# 兩者其實互斥（pipe vs 逗號）—— markdown 先嗅探保證真 markdown 表格不被 csv 誤搶；既有 csv
# fixture 無 pipe → markdown 不認領、零回歸。
MARKDOWN = Strategy("markdown", _md_applies, _md_squeeze)


# ── M19 — base64/hex blob 內容感知策略（第八片；首片「字元範圍」壓縮）──
#
# 對象：單行巨型編碼 blob（data URI 內嵌圖片、base64 編碼附件、長 hex dump、JWT 等）。LLM
# context 裡這類 blob 動輒數千字元、token 爆量，但中段對推理是不透明噪音；保頭尾足以辨識，
# 中段交給 CCR store 可逆取回。
#
# ⭐ 與前七片的根本差別：這是**第一片「字元範圍（intra-line）」策略** —— 前七片全是「行級 /
# array 元素級」丟整段；blob 策略在「一行之內」按字元偏移切頭尾。手法：找最長的「連續 blob
# 字元串」（base64/base64url/hex 字元集，**不含換行/空白** = 單一 token），保前 BLOB_HEAD +
# 後 BLOB_TAIL 字元、中段塞 marker，串外 bytes 照抄。
#
# ⭐ parity 正解：字元範圍切片在 Python（依 code point）與 Rust（依 byte）天生會分岔 —— 解法是
# **要求整段 text 為純 ASCII**（`text.isascii()`）。ASCII 下 code point 與 byte 一對一，兩語言
# 切片偏移完全一致；非 ASCII 一律不認領、落 truncate 兜底（保守、安全）。blob 本就純 ASCII。
#
# 嗅探（保守、強訊號防誤判）：連續 blob 字元串（無空白/換行/標點打斷）須 >= MIN_BLOB_RUN。散文
# 不可能有 512 字元不含空白的連續串；含標點的 minified 程式碼、URL 也會被 `.;,?&{}()` 等非 blob
# 字元打斷。tie-break 取最長；同長取最前（嚴格大於才替換）→ Py/Rs 一致。
#
# 限制（誠實記錄）：只認單行 blob（run 不跨換行）→ MIME/PEM 換行折疊的多行 base64 不認領，
# 留作未來擴充。收存點對稱 M15/M17：核心純函式回 Option、put 在呼叫端（_blob_squeeze）。

MIN_BLOB_RUN = 512  # 連續 blob 字元串長度下限 —— 低於此不認領（保守防誤判）
BLOB_HEAD = 64  # 保留 blob 開頭字元數
BLOB_TAIL = 64  # 保留 blob 結尾字元數


def _is_blob_char(b: int) -> bool:
    """base64 / base64url / hex 字元集：ASCII 英數 + `+` `/` `=` `_` `-`（純 byte，parity 安全）。"""
    return (
        0x30 <= b <= 0x39  # 0-9
        or 0x41 <= b <= 0x5A  # A-Z
        or 0x61 <= b <= 0x7A  # a-z
        or b in (0x2B, 0x2F, 0x3D, 0x5F, 0x2D)  # + / = _ -
    )


def _longest_blob_run(data: bytes) -> tuple[int, int]:
    """回最長連續 blob 字元串的 (start, end)；無則 (0, 0)。同長取最前（嚴格大於才替換）。"""
    best_s = best_e = 0
    i, n = 0, len(data)
    while i < n:
        if _is_blob_char(data[i]):
            j = i
            while j < n and _is_blob_char(data[j]):
                j += 1
            if (j - i) > (best_e - best_s):
                best_s, best_e = i, j
            i = j
        else:
            i += 1
    return best_s, best_e


def _blob_squeeze_core(text: str) -> str | None:
    """純函式：找最長 blob 串、保頭尾字元、中段塞 marker。壓不動回 None。

    非 ASCII 一律回 None —— 字元範圍切片需 code point == byte 才能 Py/Rs 一致。
    """
    if not text.isascii():
        return None
    data = text.encode("ascii")  # ASCII 下 byte index == char index → 切片偏移兩語言一致
    start, end = _longest_blob_run(data)
    run_len = end - start
    if run_len < MIN_BLOB_RUN:  # MIN_BLOB_RUN > HEAD+TAIL → dropped 必為正、無下溢
        return None
    dropped = run_len - BLOB_HEAD - BLOB_TAIL
    digest = content_key(text)
    marker = f"[... headroom-lite dropped {dropped} blob chars | sha256:{digest} ...]"
    head = text[start : start + BLOB_HEAD]
    tail = text[end - BLOB_TAIL : end]
    return text[:start] + head + marker + tail + text[end:]


def _blob_applies(text: str) -> bool:
    """嗅探：純 ASCII、且最長連續 blob 串 >= MIN_BLOB_RUN 才認領。"""
    return _blob_squeeze_core(text) is not None


def _blob_squeeze(text: str, store=None) -> str:
    """保 blob 頭尾字元；壓不動 → 原文回、絕不 put（神聖時機契約）。"""
    out = _blob_squeeze_core(text)
    if out is None:
        return text
    if store is not None:
        store.put(text)  # 產出有損輸出前先收存原文 —— 永不丟資料
    return out


# blob 排在 csv 之後、truncate 之前：極專一（需 512 字元連續 blob 串 + 純 ASCII），排最末才
# 安全；單行 blob 無換行 → log/search/csv/markdown/stacktrace（皆需多行）全不認領、diff 需 @@、
# json 需 `[`/`{` 開頭 → 都讓過，blob 接住「否則落 truncate 卻因單行無法壓」的巨型 blob。
BLOB = Strategy("blob", _blob_applies, _blob_squeeze)


# ── M20 — HTML/XML 內容感知策略（第九片「嗅探→壓」內容策略）──
#
# 對象：HTML/XML 文件（網頁爬取結果）。噪音 = `<script>`/`<style>` 元素的內文（巨型 inline
# JS/CSS bundle）+ `<!-- -->` 註解；訊號 = 標籤結構與可見文字。壓法：保留每個噪音區的「邊界」
# （script/style 的開閉標籤、註解的 `<!--`/`-->`），把內文換成單一 marker（CCR store 可逆）。
#
# 與盲目頭尾截斷的差別：truncate 不懂「script 內文是噪音、結構與文字是訊號」——巨型 inline JS
# 一多就把頁面真正內容擠出視窗；HTML 策略精準只挖掉 script/style/comment 內文、保住結構與文字。
#
# ⭐ parity（沿用 M15 JSON 模式，非 ASCII 安全）：Python 用 **char index** find/slice、Rust 用
# **byte index**，各自原生索引定位同一邏輯位置（同一個 `<script>`、同一個 `>`）→ 切出的邏輯子
# 字串相同、最終輸出 bytes 一致。故非 ASCII 文字內容（中文網頁）逐字保留、兩語言逐字節一致。
# 標籤名只比對小寫（`<script`/`<style`，保守、避開 unicode lower() 改變長度的 index 陷阱）。
#
# 嗅探（保守、強訊號）：須存在至少一個「有正確閉合、內文 >= MIN_HTML_NOISE」的噪音區。散文/
# 其他結構（log/diff/json/csv/markdown/blob）無 `<script`/`<style`/`<!--` 噪音區 → 不認領。
# 收存點對稱 M15：核心純函式回 Option、put 在呼叫端（_html_squeeze）。

MIN_HTML_NOISE = 256  # 噪音區內文長度下限 —— 低於此不挖（marker 開銷不划算）
_HTML_NOISE_TAGS = ("script", "style")  # raw-text 元素：內文為噪音；只比對小寫（保守）


def _html_noise_regions(text: str) -> list[tuple[int, int]]:
    """回所有「可挖」噪音區的 (inner_start, inner_end)，保序、不重疊、內文 >= MIN_HTML_NOISE。

    噪音區 = `<script ...>內文</script>` / `<style ...>內文</style>` / `<!--內文-->`。
    只比對小寫標籤、native char index（Rust 端對應 byte index）→ 兩語言定位同一邏輯位置。
    """
    regions: list[tuple[int, int]] = []
    i, n = 0, len(text)
    while i < n:
        # 找最早出現的噪音開頭：<script / <style / <!--
        best_pos = -1
        best_kind = ""
        for tag in _HTML_NOISE_TAGS:
            p = text.find("<" + tag, i)
            if p != -1 and (best_pos == -1 or p < best_pos):
                best_pos, best_kind = p, tag
        pc = text.find("<!--", i)
        if pc != -1 and (best_pos == -1 or pc < best_pos):
            best_pos, best_kind = pc, "<!--"
        if best_pos == -1:
            break

        if best_kind == "<!--":
            inner_start = best_pos + 4  # len("<!--")
            close = text.find("-->", inner_start)
            if close == -1:
                break  # 未終結註解 → 停（保守，不挖）
            inner_end = close
            nxt = close + 3
        else:
            gt = text.find(">", best_pos)
            if gt == -1:
                break  # 開標籤未閉合 → 停
            inner_start = gt + 1
            close = text.find("</" + best_kind, inner_start)
            if close == -1:
                i = gt + 1  # 找不到閉標籤 → 跳過此開頭、保證前進
                continue
            inner_end = close
            nxt = close  # 從閉標籤處續掃（`</tag` 不會誤配 `<tag`）

        if inner_end - inner_start >= MIN_HTML_NOISE:
            regions.append((inner_start, inner_end))
        i = nxt if nxt > i else i + 1  # 保證前進，杜絕無窮迴圈
    return regions


def _html_squeeze_core(text: str) -> str | None:
    """純函式：把每個噪音區的內文換成 marker、保留邊界與結構。無可挖區回 None。"""
    regions = _html_noise_regions(text)
    if not regions:
        return None
    digest = content_key(text)
    parts: list[str] = []
    prev = 0
    for start, end in regions:
        parts.append(text[prev:start])  # 邊界 + 結構（含非 ASCII 文字）逐字保留
        dropped = end - start
        parts.append(f"[... headroom-lite dropped {dropped} html noise chars | sha256:{digest} ...]")
        prev = end
    parts.append(text[prev:])
    return "".join(parts)


def _html_applies(text: str) -> bool:
    """嗅探：存在至少一個內文 >= MIN_HTML_NOISE 的 script/style/comment 噪音區才認領。"""
    return _html_squeeze_core(text) is not None


def _html_squeeze(text: str, store=None) -> str:
    """挖掉 script/style/comment 內文；無可挖 → 原文回、絕不 put（神聖時機契約）。"""
    out = _html_squeeze_core(text)
    if out is None:
        return text
    if store is not None:
        store.put(text)  # 產出有損輸出前先收存原文 —— 永不丟資料
    return out


# html 排在 csv 之後、blob 之前：含 inline script 的頁面該走 HTML（保結構）而非被 blob 當巨串
# 吞掉；data URI 無 `<script`/`<style`/`<!--` → HTML 不認領、落 blob。既有 fixture 皆無噪音區
# → HTML 不認領、零回歸。
HTML = Strategy("html", _html_applies, _html_squeeze)

# 策略註冊表：按優先序排列。json/diff/search/log/stacktrace/markdown/csv/html/blob 先嗅探，不命中才落 truncate 兜底。
STRATEGIES: tuple[Strategy, ...] = (
    JSON,
    DIFF,
    SEARCH,
    LOG,
    STACKTRACE,
    MARKDOWN,
    CSV,
    HTML,
    BLOB,
    TRUNCATE,
)


def squeeze_text(text: str, store=None, strategies: tuple[Strategy, ...] = STRATEGIES) -> str:
    """dispatcher：選第一個 applies 命中的策略來壓，命中即停。

    strategies 預設為模組級註冊表；測試可注入自訂順序驗證 dispatch 行為。
    無任何策略命中（理論上不會，truncate 是 catch-all）→ 防禦性回原文。
    """
    for strategy in strategies:
        if strategy.applies(text):
            return strategy.squeeze(text, store)
    return text
