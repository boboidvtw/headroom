"""M3 安全網：cache 穩定化（Phase E 之魂）。

  1. tools 正規化（E1/E2）：client 的 tool 順序/schema key 順序
     怎麼漂移，輸出 bytes 都收斂成同一份 —— 確定性正規化。
  2. cache_control 自動放置（E3）：client 沒放標記才補；
     已有任何標記 = 客戶意圖，神聖不可侵犯。
  3. 老規矩：確定性、沒事做回原始 bytes 本人、壞輸入原樣放行。
"""

import json

from headroom_lite.cache_stabilization import stabilize_request

TOOL_READ = {
    "name": "read_file",
    "description": "讀檔",
    "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
}
TOOL_WRITE = {
    "name": "write_file",
    "description": "寫檔",
    # key 順序故意亂放（type 在 properties 前、path 在 content 前）
    # —— 正規化後要遞迴排序
    "input_schema": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}},
}


def _body(tools: list, *, system=None, messages=None) -> bytes:
    body = {"model": "claude-opus-4-8", "tools": tools}
    if system is not None:
        body["system"] = system
    body["messages"] = messages or [{"role": "user", "content": "hi"}]
    return json.dumps(body, ensure_ascii=False).encode("utf-8")


# ------------------------------------------------------- tools 正規化

def test_flickering_tool_order_converges_to_same_bytes():
    """殺手測試：同一組 tools、兩種順序 → 穩定化後 bytes 一模一樣。"""
    turn_a = _body([TOOL_READ, TOOL_WRITE])
    turn_b = _body([TOOL_WRITE, TOOL_READ])
    assert turn_a != turn_b                    # 入口確實不同
    assert stabilize_request(turn_a) == stabilize_request(turn_b)  # 出口收斂


def test_schema_keys_sorted_recursively():
    out = json.loads(stabilize_request(_body([TOOL_WRITE])))
    schema = out["tools"][0]["input_schema"]
    assert list(schema.keys()) == sorted(schema.keys())
    assert list(schema["properties"].keys()) == sorted(schema["properties"].keys())


# --------------------------------------------- cache_control 自動放置

def _system_blocks():
    return [{"type": "text", "text": "你是嚴謹的助理。"}]


def _three_turns():
    return [
        {"role": "user", "content": [{"type": "text", "text": "第一問"}]},
        {"role": "assistant", "content": [{"type": "text", "text": "第一答"}]},
        {"role": "user", "content": [{"type": "text", "text": "第二問（live zone）"}]},
    ]


def test_auto_breakpoints_when_client_has_none():
    raw = _body([TOOL_READ], system=_system_blocks(), messages=_three_turns())
    out = json.loads(stabilize_request(raw))

    # system 最後一個 block 拿到標記（涵蓋 tools + system 前綴）
    assert out["system"][-1]["cache_control"] == {"type": "ephemeral"}
    # live zone 之前的最後一則訊息（assistant）最後一個 block 拿到標記
    assert out["messages"][-2]["content"][-1]["cache_control"] == {"type": "ephemeral"}
    # live zone 本身不放（它還會變，放了也快取不到東西）
    assert "cache_control" not in out["messages"][-1]["content"][-1]


def test_existing_markers_are_sacred():
    """client 已放任何標記 → 我們一個都不加、也不動既有的。"""
    messages = _three_turns()
    messages[0]["content"][0]["cache_control"] = {"type": "ephemeral"}
    raw = _body([TOOL_READ], system=_system_blocks(), messages=messages)
    out = json.loads(stabilize_request(raw))

    marks = [
        (i, j)
        for i, m in enumerate(out["messages"])
        for j, b in enumerate(m["content"])
        if isinstance(b, dict) and "cache_control" in b
    ]
    assert marks == [(0, 0)]                       # 只剩 client 自己放的那一個
    assert all("cache_control" not in b for b in out.get("system", []))


# ------------------------------------------------------------- 老規矩

def test_already_canonical_returns_original_bytes():
    """沒事可做（tools 已排序、標記已存在）→ 回傳原始 bytes 本人。"""
    canonical_tool = {
        "name": "read_file",
        "description": "讀檔",
        # schema key 已是遞迴排序狀態（properties < type）
        "input_schema": {"properties": {"path": {"type": "string"}}, "type": "object"},
    }
    messages = _three_turns()
    messages[0]["content"][0]["cache_control"] = {"type": "ephemeral"}
    raw = _body([canonical_tool], messages=messages)
    assert stabilize_request(raw) == raw


def test_deterministic():
    raw = _body([TOOL_WRITE, TOOL_READ], system=_system_blocks(), messages=_three_turns())
    assert stabilize_request(raw) == stabilize_request(raw)


def test_bad_input_passes_through():
    assert stabilize_request(b"not json") == b"not json"
