"""M1 — live-zone 壓縮引擎（Phase B 之魂）。

只壓「最後一個 user turn 裡的 tool_result」，其餘一概不碰。

為什麼只有這裡能壓？（兩層洞察）
  1. live zone 是上游 cache「還沒見過」的區域 —— 壓它不會 miss 任何前綴。
  2. 但 client 每輪重送完整歷史，所以壓縮必須是「確定性函式」：
     相同輸入 bytes → 永遠相同輸出 bytes。第 N+1 輪重壓同一個
     tool_result 時，結果必須跟第 N 輪一個 byte 不差，上游看到的
     歷史才會穩定。因此：無時間戳、無隨機、hash-keyed。

失敗模式契約：壓縮永遠不准弄壞請求。任何解析失敗、結構不符、
壓了沒賺 —— 一律回傳「原始 bytes 本人」（連重新序列化都不做）。
"""

from __future__ import annotations

import hashlib
import json

# 只有「夠大」的 tool_result 才值得壓：太小的省不了幾個 token，
# 卻要付出重新序列化整個 body 的 cache 風險。門檻對齊計劃 B4 的精神。
MIN_COMPRESSIBLE_BYTES = 2048

# 確定性截斷參數：保留頭尾，中段以標記取代。
HEAD_LINES = 20
TAIL_LINES = 10


def _squeeze_text(text: str) -> str:
    """確定性壓縮一段長文字：頭 + 標記 + 尾。

    標記內含「原文 SHA-256 前 16 碼 + 省略行數」：
      - hash 讓同一份原文永遠產生同一個標記（確定性的證明書），
        也讓日後 CCR（可逆取回）能用 hash 當 key 找回原文。
      - 絕不放時間戳或隨機值 —— 那會讓第 N+1 輪重壓結果不同。
    """
    lines = text.splitlines()
    if len(lines) <= HEAD_LINES + TAIL_LINES:
        return text  # 行數太少，沒得壓

    digest = hashlib.sha256(text.encode("utf-8")).hexdigest()[:16]
    omitted = len(lines) - HEAD_LINES - TAIL_LINES
    marker = f"[... headroom-lite squeezed {omitted} lines | sha256:{digest} ...]"
    return "\n".join([*lines[:HEAD_LINES], marker, *lines[-TAIL_LINES:]])


def _compress_block(block: dict) -> dict:
    """壓一個 content block。非 tool_result 或不夠大 → 原物件原樣回傳。

    回傳新 dict（不可變風格），絕不就地修改輸入。
    """
    if block.get("type") != "tool_result":
        return block
    content = block.get("content")
    if not isinstance(content, str):
        return block  # 結構化 content（list 形式）M1 先不處理，原樣保留
    if len(content.encode("utf-8")) < MIN_COMPRESSIBLE_BYTES:
        return block

    squeezed = _squeeze_text(content)
    if len(squeezed) >= len(content):
        return block  # 驗證 + fallback：沒賺就不動
    return {**block, "content": squeezed}


def compress_request(raw: bytes) -> bytes:
    """入口：對 /v1/messages 的 body bytes 做 live-zone 壓縮。

    只在「真的壓到東西」時才重新序列化；否則回傳原始 bytes 本人。
    """
    try:
        body = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        return raw  # 失敗模式契約：壞輸入原樣放行

    messages = body.get("messages") if isinstance(body, dict) else None
    if not isinstance(messages, list) or not messages:
        return raw

    last = messages[-1]
    # live zone 定義：最後一則訊息、必須是 user、content 是 block list。
    # 不是 user 結尾（少見）就整包放行 —— 保守優先。
    if not isinstance(last, dict) or last.get("role") != "user":
        return raw
    content = last.get("content")
    if not isinstance(content, list):
        return raw

    new_blocks = [_compress_block(b) if isinstance(b, dict) else b for b in content]
    if all(nb is ob for nb, ob in zip(new_blocks, content)):
        return raw  # 一個 block 都沒壓到 → 原始 bytes 直接回家

    # 只重建被動到的路徑（messages[-1].content），其餘子樹原物件共用。
    new_body = {
        **body,
        "messages": [*messages[:-1], {**last, "content": new_blocks}],
    }
    # 序列化必須「規範化」：固定 separators、不做 ASCII escape。
    # 同輸入 → 同輸出的最後一塊拼圖。
    return json.dumps(new_body, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
