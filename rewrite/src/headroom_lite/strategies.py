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

# 策略註冊表：按優先序排列。log 先嗅探，不命中才落到 truncate 兜底。
STRATEGIES: tuple[Strategy, ...] = (LOG, TRUNCATE)


def squeeze_text(text: str, store=None, strategies: tuple[Strategy, ...] = STRATEGIES) -> str:
    """dispatcher：選第一個 applies 命中的策略來壓，命中即停。

    strategies 預設為模組級註冊表；測試可注入自訂順序驗證 dispatch 行為。
    無任何策略命中（理論上不會，truncate 是 catch-all）→ 防禦性回原文。
    """
    for strategy in strategies:
        if strategy.applies(text):
            return strategy.squeeze(text, store)
    return text
