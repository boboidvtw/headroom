"""M2 安全網：byte-level SSE 狀態機。

對應 REALIGNMENT Phase C 之魂（P1-8 / P1-9）：
  1. chunk 可以切在任何 byte —— 包括 UTF-8 字元中間、事件中間。
     必須緩衝 bytes、只在完整事件邊界 decode。
  2. 所有 delta 型別都要有歸位：text / thinking / signature /
     input_json。漏接 = 內容靜默遺失。
  3. ping 忽略、error 事件浮上來，不准 panic。
"""

import json

from headroom_lite.sse import MessageAccumulator, SSEByteSplitter


def _event(name: str, data: dict) -> bytes:
    return f"event: {name}\ndata: {json.dumps(data, ensure_ascii=False)}\n\n".encode("utf-8")


# ---------------------------------------------------------------- splitter

def test_emoji_split_across_chunks_preserved():
    """殺手測試：🔥(f0 9f 94 a5) 被網路切成兩半，一個 byte 都不准掉。

    這正是原版 `chunk.decode("utf-8", errors="ignore")` 會靜默吞字的場景。
    """
    raw = _event("content_block_delta", {"delta": {"type": "text_delta", "text": "前🔥後"}})
    cut = raw.index(b"\xf0\x9f") + 2  # 正好切在 emoji 的 4 bytes 中間

    splitter = SSEByteSplitter()
    events = splitter.feed(raw[:cut])
    assert events == []  # 事件還沒完整，什麼都不該吐
    events = splitter.feed(raw[cut:])
    assert len(events) == 1
    assert "前🔥後" in events[0].decode("utf-8")  # 完整事件 decode 後 emoji 完好


def test_event_split_across_chunks():
    raw = _event("message_start", {"type": "message_start"})
    splitter = SSEByteSplitter()
    assert splitter.feed(raw[:10]) == []
    assert splitter.feed(raw[10:]) == [raw[:-2]]  # 吐出的事件不含結尾空行


def test_multiple_events_in_one_chunk():
    e1 = _event("ping", {"type": "ping"})
    e2 = _event("message_stop", {"type": "message_stop"})
    splitter = SSEByteSplitter()
    assert splitter.feed(e1 + e2) == [e1[:-2], e2[:-2]]


def test_byte_by_byte_worst_case():
    """最壞情況：一次餵一個 byte，事件仍要完整重組。"""
    raw = _event("content_block_delta", {"delta": {"type": "text_delta", "text": "🔥 你好"}})
    splitter = SSEByteSplitter()
    collected = [ev for b in (raw[i:i + 1] for i in range(len(raw))) for ev in splitter.feed(b)]
    assert collected == [raw[:-2]]


# ------------------------------------------------------------ accumulator

def _full_stream() -> list[bytes]:
    """模擬一個含 thinking + text 兩個 block 的真實串流。"""
    return [
        _event("message_start", {"type": "message_start"}),
        _event("content_block_start",
               {"type": "content_block_start", "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}}),
        _event("content_block_delta",
               {"type": "content_block_delta", "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "先想一下…"}}),
        _event("content_block_delta",
               {"type": "content_block_delta", "index": 0,
                "delta": {"type": "signature_delta", "signature": "sig_abc123"}}),
        _event("content_block_stop", {"type": "content_block_stop", "index": 0}),
        _event("content_block_start",
               {"type": "content_block_start", "index": 1,
                "content_block": {"type": "text", "text": ""}}),
        _event("content_block_delta",
               {"type": "content_block_delta", "index": 1,
                "delta": {"type": "text_delta", "text": "答案是"}}),
        _event("content_block_delta",
               {"type": "content_block_delta", "index": 1,
                "delta": {"type": "text_delta", "text": " 42 🔥"}}),
        _event("content_block_stop", {"type": "content_block_stop", "index": 1}),
        _event("ping", {"type": "ping"}),
        _event("message_stop", {"type": "message_stop"}),
    ]


def test_accumulator_rebuilds_blocks_by_index():
    acc = MessageAccumulator()
    splitter = SSEByteSplitter()
    for chunk in _full_stream():
        for ev in splitter.feed(chunk):
            acc.consume(ev)

    blocks = acc.blocks()
    assert blocks[0]["type"] == "thinking"
    assert blocks[0]["thinking"] == "先想一下…"
    assert blocks[0]["signature"] == "sig_abc123"  # P1-9：signature 不准掉
    assert blocks[1]["type"] == "text"
    assert blocks[1]["text"] == "答案是 42 🔥"


def test_unknown_delta_type_does_not_crash_and_is_recorded():
    """前向相容：沒見過的 delta 型別不准 panic，但要留下紀錄。"""
    acc = MessageAccumulator()
    acc.consume(_event("content_block_start",
                       {"type": "content_block_start", "index": 0,
                        "content_block": {"type": "text", "text": ""}})[:-2])
    acc.consume(_event("content_block_delta",
                       {"type": "content_block_delta", "index": 0,
                        "delta": {"type": "future_delta", "stuff": "?"}})[:-2])
    assert acc.unknown_delta_types == ["future_delta"]


def test_error_event_surfaces():
    acc = MessageAccumulator()
    acc.consume(_event("error", {"type": "error",
                                 "error": {"type": "overloaded_error", "message": "busy"}})[:-2])
    assert acc.error == {"type": "overloaded_error", "message": "busy"}
