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

# 策略註冊表：按優先序排列。骨架階段只有 truncate；接內容感知策略時插在它前面。
STRATEGIES: tuple[Strategy, ...] = (TRUNCATE,)


def squeeze_text(text: str, store=None, strategies: tuple[Strategy, ...] = STRATEGIES) -> str:
    """dispatcher：選第一個 applies 命中的策略來壓，命中即停。

    strategies 預設為模組級註冊表；測試可注入自訂順序驗證 dispatch 行為。
    無任何策略命中（理論上不會，truncate 是 catch-all）→ 防禦性回原文。
    """
    for strategy in strategies:
        if strategy.applies(text):
            return strategy.squeeze(text, store)
    return text
