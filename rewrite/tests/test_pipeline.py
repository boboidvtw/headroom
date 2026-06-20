"""M8 安全網：lazy registration —— pipeline 的「壓到才註冊」治本。

對應 live traffic 實測抓到的真 bug（2026-06-12）：
  register_ccr_tool「每請求都註冊」會在 tools 陣列（cache 前綴最前面）
  加一個工具，害上游對 raw 流量的部分命中容錯失效。

M8 治本：把註冊決策從 building block 上移到 orchestration 層。
新順序 stabilize → compress →「有壓到才 register」：
  1. 多數請求不壓縮 → tools 全程不動 → 零 cache 影響（lazy 的核心）。
  2. 真的壓到了才註冊 ccr_retrieve；接在 tools 尾端、不重排
     —— prefix cache 逐字節前綴比對，擺尾端讓 client 既有 tools
     維持 byte-identical，divergence point 往後推、保住更多前綴。
  3. 失敗模式契約穿透整條 pipeline：壞輸入 → 原始 bytes 本人。
"""

import json

from headroom_lite.ccr import CCRStore
from headroom_lite.pipeline import process_request

# 一段夠長、可壓縮的假 tool 輸出（> MIN_COMPRESSIBLE_BYTES 且行數夠多）
HUGE_LOG = "\n".join(f"2026-06-11T10:00:{i:02d} INFO worker heartbeat ok" for i in range(60)) * 5


def _body(*, tool_output: str, tools=None) -> bytes:
    """單輪 user-turn 請求；tool_output 放進 live zone 的 tool_result。"""
    body = {
        "model": "claude-opus-4-8",
        "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": tool_output},
            ]},
        ],
    }
    if tools is not None:
        body["tools"] = tools
    return json.dumps(body, ensure_ascii=False).encode("utf-8")


def _ccr_names(out_bytes: bytes) -> list[str]:
    tools = json.loads(out_bytes).get("tools", [])
    return [t.get("name") for t in tools]


# --------------------------------------------------- lazy 的核心：沒壓 = 不註冊

def test_no_compression_means_no_ccr_registration():
    """M8 頭號契約：這輪沒壓到任何東西 → tools 不長出 ccr_retrieve。"""
    tiny = _body(
        tool_output="short output",  # 遠小於門檻，不會壓
        tools=[{"name": "read_file", "description": "讀檔",
                "input_schema": {"type": "object"}}],
    )
    out = process_request(tiny, store=CCRStore())
    assert "ccr_retrieve" not in _ccr_names(out)


def test_client_tools_untouched_when_nothing_compressed():
    """沒壓縮時，已是規範形（排序過）的 client tools 一個 byte 都不動。"""
    canonical = _body(
        tool_output="short",
        tools=[{"name": "read_file", "description": "讀檔",
                "input_schema": {"type": "object"}}],
    )
    # 先過一次 pipeline 拿到規範形，再餵回去應冪等回傳原始 bytes 本人
    once = process_request(canonical, store=CCRStore())
    twice = process_request(once, store=CCRStore())
    assert twice is once  # 原始 bytes 本人（identity）—— 連重新序列化都不做


def test_stabilization_alone_does_not_register():
    """只觸發 stabilize（亂序 tools 要排序）但沒壓縮 → 排序了、但不註冊 ccr。"""
    unsorted = _body(
        tool_output="short",
        tools=[
            {"name": "write_file", "description": "寫檔", "input_schema": {"type": "object"}},
            {"name": "read_file", "description": "讀檔", "input_schema": {"type": "object"}},
        ],
    )
    out = process_request(unsorted, store=CCRStore())
    names = _ccr_names(out)
    assert names == ["read_file", "write_file"]  # 排序了
    assert "ccr_retrieve" not in names           # 但沒壓 → 不註冊


# --------------------------------------------------- 有壓到 → 才註冊

def test_compression_triggers_ccr_registration():
    """真的壓到了 → ccr_retrieve 必須在，且原文可從 store 取回。"""
    big = _body(tool_output=HUGE_LOG, tools=[
        {"name": "read_file", "description": "讀檔", "input_schema": {"type": "object"}},
    ])
    store = CCRStore()
    out = process_request(big, store=store)

    assert "ccr_retrieve" in _ccr_names(out)
    # 壓縮確實發生：live zone 被縮、原文進了 store。
    # 不綁特定策略 —— HUGE_LOG 是純 log，M12 起走 log 策略（"dropped"），
    # 其他內容走 truncate（"squeezed"）；兩者共用標記前綴，斷言意圖＝壓到了。
    squeezed = json.loads(out)["messages"][0]["content"][0]["content"]
    assert "[... headroom-lite " in squeezed
    assert len(store) == 1


def test_ccr_appended_at_end_not_sorted_to_front():
    """cache 最佳化：ccr_retrieve 接在 client tools 尾端、不排到最前面。

    client 既有 tools 維持 byte-identical，divergence point 往後推。
    """
    big = _body(tool_output=HUGE_LOG, tools=[
        {"name": "read_file", "description": "讀檔", "input_schema": {"type": "object"}},
        {"name": "write_file", "description": "寫檔", "input_schema": {"type": "object"}},
    ])
    out = process_request(big, store=CCRStore())
    names = _ccr_names(out)
    assert names == ["read_file", "write_file", "ccr_retrieve"]  # client tools 排序、ccr 殿後


# --------------------------------------------------- 失敗模式 + 確定性

def test_bad_input_passes_through():
    """壞輸入 → 原始 bytes 本人（穿透整條 pipeline）。"""
    assert process_request(b"not json", store=CCRStore()) == b"not json"


def test_pipeline_is_deterministic():
    """同輸入兩次 → 逐字節相同（client 每輪重送完整歷史的前提）。"""
    big = _body(tool_output=HUGE_LOG, tools=[
        {"name": "read_file", "description": "讀檔", "input_schema": {"type": "object"}},
    ])
    assert process_request(big) == process_request(big)
