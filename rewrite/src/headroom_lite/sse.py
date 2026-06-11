"""M2 — byte-level SSE 狀態機（Phase C 之魂）。

兩層分工：
  SSEByteSplitter    byte 層。緩衝任意爛碎片，只在完整事件邊界
                     （空行）切開吐出。從頭到尾不 decode —— 半截
                     的 UTF-8 字元安全地躺在緩衝區等下一個 chunk。
  MessageAccumulator 事件層。decode「完整事件」、用 index 把各種
                     delta 歸位到正確 block，重建完整訊息。

為什麼不能邊收邊 decode？
  TCP chunk 可以切在 🔥(f0 9f 94 a5) 的第 2 byte。原版
  `chunk.decode("utf-8", errors="ignore")` 會把兩個半截都靜默
  丟掉（P1-8）。byte 緩衝 + 完整後才 decode，問題從根本消失。
"""

from __future__ import annotations

import json

# SSE 規格允許 \r\n 或 \n 當行尾；事件邊界是「空行」。
# 學習版支援最常見的兩種：\n\n 與 \r\n\r\n。
_BOUNDARIES = (b"\r\n\r\n", b"\n\n")


class SSEByteSplitter:
    """增量切割器：feed 進任意 bytes 碎片，吐出完整事件的 bytes。"""

    def __init__(self) -> None:
        self._buffer = b""

    def feed(self, chunk: bytes) -> list[bytes]:
        """吞下一個 chunk，回傳「此刻已完整」的事件 bytes 清單。

        不完整的尾巴留在內部緩衝區 —— 包括被切一半的 UTF-8 字元。
        """
        self._buffer += chunk
        events: list[bytes] = []

        while True:
            # 找最早出現的事件邊界（兩種行尾都要看）
            cut = -1
            sep_len = 0
            for sep in _BOUNDARIES:
                idx = self._buffer.find(sep)
                if idx != -1 and (cut == -1 or idx < cut):
                    cut, sep_len = idx, len(sep)
            if cut == -1:
                return events  # 沒有完整事件了，剩的繼續緩衝

            events.append(self._buffer[:cut])
            self._buffer = self._buffer[cut + sep_len:]


def _parse_event(raw: bytes) -> dict | None:
    """把一個「完整事件」的 bytes 解析成 data 的 JSON dict。

    這裡才允許 decode —— 事件已完整，UTF-8 必然完整。
    多行 data: 依 SSE 規格以 \n 串接。無 data 或非 JSON → None。
    """
    data_lines = [
        line.split(b":", 1)[1].lstrip(b" ")
        for line in raw.replace(b"\r\n", b"\n").split(b"\n")
        if line.startswith(b"data:")
    ]
    if not data_lines:
        return None
    payload = b"\n".join(data_lines)
    if payload == b"[DONE]":  # OpenAI 風格結束哨兵
        return None
    try:
        parsed = json.loads(payload)
    except ValueError:
        return None
    return parsed if isinstance(parsed, dict) else None


class MessageAccumulator:
    """index-keyed block 重建器。

    Anthropic 串流的契約：每個 content block 有自己的 index，
    delta 透過 index 歸位。漏接任何 delta 型別 = 該內容靜默遺失，
    所以未知型別要記錄下來（前向相容 + 可觀測）。
    """

    def __init__(self) -> None:
        self._blocks: dict[int, dict] = {}
        self.unknown_delta_types: list[str] = []
        self.error: dict | None = None

    def consume(self, raw_event: bytes) -> None:
        data = _parse_event(raw_event)
        if data is None:
            return
        kind = data.get("type")

        if kind == "error":
            self.error = data.get("error")
        elif kind == "content_block_start":
            # 以 start 給的骨架當底；copy 避免共享可變狀態
            self._blocks[data["index"]] = dict(data.get("content_block") or {})
        elif kind == "content_block_delta":
            self._apply_delta(data["index"], data.get("delta") or {})
        # message_start / content_block_stop / message_stop / ping：
        # 學習版無事可做，明確列出代表「已知且刻意忽略」。

    def _apply_delta(self, index: int, delta: dict) -> None:
        block = self._blocks.setdefault(index, {})
        kind = delta.get("type")

        # 每種 delta 都「明確」有自己的分支 —— 計劃 C5 的教訓：
        # 靠 catch-all 碰巧沒壞，遲早會壞。
        if kind == "text_delta":
            block["text"] = block.get("text", "") + delta.get("text", "")
        elif kind == "thinking_delta":
            block["thinking"] = block.get("thinking", "") + delta.get("thinking", "")
        elif kind == "signature_delta":
            # P1-9：signature 一掉，這個 thinking 區塊送回上游就無效
            block["signature"] = block.get("signature", "") + delta.get("signature", "")
        elif kind == "input_json_delta":
            block["partial_json"] = block.get("partial_json", "") + delta.get("partial_json", "")
        elif kind == "citations_delta":
            block.setdefault("citations", []).append(delta.get("citation"))
        else:
            self.unknown_delta_types.append(str(kind))

    def blocks(self) -> list[dict]:
        """依 index 排序回傳重建好的 blocks（新 list，不外漏內部狀態）。"""
        return [dict(self._blocks[i]) for i in sorted(self._blocks)]
