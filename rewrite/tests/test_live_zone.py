"""M1 安全網：live-zone 壓縮引擎的四條鐵律。

對應 REALIGNMENT Phase B 之魂：
  1. 只壓 live zone（最後一個 user turn 的 tool_result）。
  2. 舊 turn 的內容與位置完全保留。
  3. 確定性：相同輸入 bytes → 相同輸出 bytes（client 每輪重送
     完整歷史，第 N+1 輪必須把同樣的舊 tool_result 壓成同樣的結果）。
  4. 驗證 + fallback：壓了沒變小 → 直接回傳「原始 bytes」（連重新
     序列化都不做 —— passthrough is sacred）。
"""

import json

from headroom_lite.live_zone import compress_request

# 一段夠長、可壓縮的假 tool 輸出（重複的 log 行）
HUGE_LOG = "\n".join(f"2026-06-11T10:00:{i:02d} INFO worker heartbeat ok" for i in range(60)) * 5


def _conversation(latest_tool_output: str) -> dict:
    """三輪對話：舊 turn 也有一個大 tool_result（它不准被碰）。"""
    return {
        "model": "claude-opus-4-8",
        "system": "你是嚴謹的助理。",
        "messages": [
            {"role": "user", "content": "幫我看 log"},
            {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "read_log", "input": {}},
                ],
            },
            {
                "role": "user",
                "content": [
                    # 舊 turn 的 tool_result —— cache 熱區，永不可動
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": HUGE_LOG},
                ],
            },
            {"role": "assistant", "content": "看完了，要再看一次嗎？"},
            {
                "role": "user",
                "content": [
                    # live zone：最後一個 user turn 的 tool_result
                    {"type": "tool_result", "tool_use_id": "tu_2", "content": latest_tool_output},
                    {"type": "text", "text": "再幫我總結這份"},
                ],
            },
        ],
    }


def _raw(body: dict) -> bytes:
    return json.dumps(body, ensure_ascii=False).encode("utf-8")


def test_live_zone_tool_result_gets_compressed():
    raw = _raw(_conversation(HUGE_LOG))
    out = compress_request(raw)

    assert len(out) < len(raw)  # 真的有壓
    parsed = json.loads(out)
    live_block = parsed["messages"][-1]["content"][0]
    assert len(live_block["content"]) < len(HUGE_LOG)


def test_old_turns_are_never_touched():
    raw = _raw(_conversation(HUGE_LOG))
    out = compress_request(raw)
    parsed = json.loads(out)

    # 舊 turn 的大 tool_result 必須一字不差（即使它又大又可壓）
    assert parsed["messages"][2]["content"][0]["content"] == HUGE_LOG
    # system、舊 user/assistant 文字也都不准變
    assert parsed["system"] == "你是嚴謹的助理。"
    assert parsed["messages"][0]["content"] == "幫我看 log"
    assert parsed["messages"][3]["content"] == "看完了，要再看一次嗎？"
    # live zone 裡「使用者親手打的字」也不准動 —— 只壓 tool_result
    assert parsed["messages"][-1]["content"][1]["text"] == "再幫我總結這份"


def test_deterministic_same_input_same_output():
    raw = _raw(_conversation(HUGE_LOG))
    assert compress_request(raw) == compress_request(raw)


def test_fallback_returns_original_bytes_when_no_gain():
    """live zone 很小 → 不壓 → 回傳的必須是『原始 bytes 本人』。

    注意斷言的是 bytes 相等，不是語意相等：不壓縮時連重新序列化
    都不允許（M0 的北極星延續到這裡）。
    """
    raw = _raw(_conversation("short output"))
    assert compress_request(raw) == raw


def test_non_json_body_passes_through():
    """壞輸入永遠不准炸：原樣放行（壓縮絕不能弄壞請求）。"""
    raw = b"this is not json at all"
    assert compress_request(raw) == raw
