"""M4 安全網：CCR 可逆取回（Compress-Cache-Retrieve）。

對應北極星鐵律 4 + 計劃 B7：
  1. 壓縮永不丟資料 —— 原文存進 content-addressed store，
     標記裡的 sha256 就是取回 key。
  2. register_ccr_tool 是「無條件」的純 building block —— 被呼叫就註冊。
     （「何時呼叫」的 lazy 決策在 pipeline 層，見 test_pipeline.py / M8。）
  3. 工具定義 bytes 跨輪逐字節穩定。
"""

import json
import re

from headroom_lite.ccr import CCRStore, register_ccr_tool
from headroom_lite.live_zone import compress_request

HUGE_LOG = "\n".join(f"2026-06-11T10:00:{i:02d} INFO worker heartbeat ok" for i in range(60)) * 5


def _body_with_big_tool_result() -> bytes:
    return json.dumps({
        "model": "claude-opus-4-8",
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": HUGE_LOG},
            ]},
        ],
    }, ensure_ascii=False).encode("utf-8")


# ----------------------------------------------------------- 可逆性

def test_squeezed_original_is_retrievable():
    """殺手測試：壓掉的內容，用標記裡的 key 一字不差取回。"""
    store = CCRStore()
    out = compress_request(_body_with_big_tool_result(), store=store)

    squeezed = json.loads(out)["messages"][0]["content"][0]["content"]
    key = re.search(r"sha256:([0-9a-f]{16})", squeezed).group(1)
    assert store.get(key) == HUGE_LOG


def test_store_is_content_addressed():
    store = CCRStore()
    k1 = store.put("same text")
    k2 = store.put("same text")
    assert k1 == k2           # 同文同 key
    assert len(store) == 1    # 自動去重
    assert store.get(k1) == "same text"
    assert store.get("0" * 16) is None  # 未知 key → None，不炸


def test_compress_without_store_still_works():
    """store 是可選的 —— M1 的行為完全不變（向後相容）。"""
    out = compress_request(_body_with_big_tool_result())
    assert len(out) < len(_body_with_big_tool_result())


# ------------------------------------------------- building block：被叫到就註冊

def test_tool_registered_even_when_nothing_compressed():
    """building block 契約：register_ccr_tool 被呼叫就無條件註冊。

    （pipeline 才負責「沒壓到就別呼叫」的 lazy 決策 —— 見 test_pipeline.py。）
    """
    tiny = json.dumps({
        "model": "claude-opus-4-8",
        "tools": [{"name": "read_file", "description": "讀檔",
                   "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": "hi"}],
    }).encode()
    out = json.loads(register_ccr_tool(tiny))
    assert any(t.get("name") == "ccr_retrieve" for t in out["tools"])


def test_tool_definition_bytes_stable_across_turns():
    """工具定義必須跨輪逐字節相同 —— 差一個 byte，tools 前綴就炸。"""
    turn_1 = json.dumps({"model": "m", "tools": [],
                         "messages": [{"role": "user", "content": "a"}]}).encode()
    turn_2 = json.dumps({"model": "m", "tools": [],
                         "messages": [{"role": "user", "content": "b"}]}).encode()

    def_1 = [t for t in json.loads(register_ccr_tool(turn_1))["tools"] if t["name"] == "ccr_retrieve"]
    def_2 = [t for t in json.loads(register_ccr_tool(turn_2))["tools"] if t["name"] == "ccr_retrieve"]
    assert json.dumps(def_1) == json.dumps(def_2)


def test_no_tools_array_gets_one_created():
    no_tools = json.dumps({"model": "m",
                           "messages": [{"role": "user", "content": "hi"}]}).encode()
    out = json.loads(register_ccr_tool(no_tools))
    assert [t["name"] for t in out["tools"]] == ["ccr_retrieve"]


def test_already_registered_returns_original_bytes():
    """已經註冊過（例如上一層 middleware 做了）→ 回原始 bytes 本人。"""
    raw = register_ccr_tool(json.dumps(
        {"model": "m", "tools": [], "messages": [{"role": "user", "content": "hi"}]}
    ).encode())
    assert register_ccr_tool(raw) == raw


def test_bad_input_passes_through():
    assert register_ccr_tool(b"not json") == b"not json"
