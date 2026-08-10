"""M23 — volatile 唯讀掃描測試（Phase E 的 observe 那一半）。

守兩件事：
  1. 該指出來的有指出來（timestamp / uuid_v4 / id_field）。
  2. 乾淨的 prompt 不得誤報 —— 觀測線一旦會亂叫，使用者就會學會忽略它，
     偵測器等於沒有。

外加一條重建特有的：整條掃描絕不改動輸入 bytes。
"""

from __future__ import annotations

import json

from headroom_lite.volatile import (
    ID_FIELD,
    MAX_FINDINGS,
    SAMPLE_MAX_CHARS,
    TIMESTAMP,
    UUID_V4,
    scan_request,
)


def _body(obj) -> bytes:
    return json.dumps(obj).encode("utf-8")


# ─── 1. timestamp ──────────────────────────────────────────────────────


def test_detects_iso8601_timestamp_in_system_prompt():
    findings = scan_request(_body({"system": "Today is 2026-05-04T14:30:00Z. Be concise."}))
    assert len(findings) == 1
    assert findings[0].kind == TIMESTAMP
    assert findings[0].location == "system"
    assert findings[0].sample == "2026-05-04T14:30:00"


def test_iso8601_with_space_separator_recognized():
    """RFC 3339 §5.6 允許用空格代替 T —— ops log 常這樣渲染。"""
    findings = scan_request(_body({"system": "started at 2026-05-04 14:30:00"}))
    assert [f.kind for f in findings] == [TIMESTAMP]


def test_non_ascii_digits_are_not_digits():
    """parity 地雷：Python `str.isdigit()` 對阿拉伯-印度數字回 True，
    Rust `u8::is_ascii_digit` 不會 —— 兩邊都必須只認 ASCII `0-9`。"""
    findings = scan_request(_body({"system": "٢٠٢٦-٠٥-٠٤T١٤:٣٠:٠٠"}))
    assert findings == []


# ─── 2. uuid v4 ────────────────────────────────────────────────────────


def test_detects_uuid_v4_in_user_message():
    findings = scan_request(
        _body(
            {
                "messages": [
                    {"role": "user", "content": "trace=550e8400-e29b-41d4-a716-446655440000"},
                    {"role": "user", "content": "and now?"},
                ]
            }
        )
    )
    assert len(findings) == 1
    assert findings[0].kind == UUID_V4
    assert findings[0].location == "messages[0].content"
    assert findings[0].sample == "550e8400-e29b-41d4-a716-446655440000"


def test_random_hex_without_v4_nibble_is_not_a_uuid():
    """判準的精髓不是「找 UUID」，是「找每次都會變的 UUID」——
    build hash / 固定識別碼不是 v4，不該被指控。"""
    findings = scan_request(
        _body(
            {
                "messages": [
                    {"role": "user", "content": "id=550e8400-e29b-01d4-a716-446655440000"},
                    {"role": "user", "content": "and now?"},
                ]
            }
        )
    )
    assert all(f.kind != UUID_V4 for f in findings)


def test_uuid_with_bad_variant_nibble_is_not_v4():
    """位置 19 的 variant nibble 必須是 8/9/a/b（RFC 4122 §4.4）。"""
    findings = scan_request(
        _body(
            {
                "messages": [
                    {"role": "user", "content": "550e8400-e29b-41d4-c716-446655440000"},
                    {"role": "user", "content": "and now?"},
                ]
            }
        )
    )
    assert all(f.kind != UUID_V4 for f in findings)


# ─── 3. ID 名稱欄位 ────────────────────────────────────────────────────


def test_detects_request_id_field_in_nested_schema():
    """補前兩條漏掉的：整數 trace ID、自訂 slug 格式。"""
    findings = scan_request(
        _body(
            {
                "tools": [
                    {
                        "name": "lookup",
                        "description": "Look up a user.",
                        "input_schema": {
                            "type": "object",
                            "properties": {
                                "user_id": {"type": "string"},
                                "request_id": "req-2026-abc-12345",
                            },
                        },
                    }
                ]
            }
        )
    )
    id_fields = [f for f in findings if f.kind == ID_FIELD]
    assert len(id_fields) == 1
    assert id_fields[0].location == "tools[0].input_schema.properties.request_id"
    assert id_fields[0].sample == "req-2026-abc-12345"


def test_id_field_with_empty_value_does_not_fire():
    """只『宣告』了 request_id 而沒填值的 schema 不該觸發。"""
    findings = scan_request(_body({"tools": [{"input_schema": {"properties": {"request_id": ""}}}]}))
    assert all(f.kind != ID_FIELD for f in findings)


def test_id_field_name_match_is_ascii_case_insensitive_substring():
    findings = scan_request(_body({"tools": [{"input_schema": {"X_Request_ID": 7}}]}))
    assert [f.kind for f in findings] == [ID_FIELD]
    assert findings[0].location == "tools[0].input_schema.X_Request_ID"


def test_numeric_id_value_sample_keeps_original_literal():
    """parity 地雷：Rust 開 arbitrary_precision 保留 `1.10`，Python 預設
    `json.loads` 會把它變成 float 再變回 `1.1` —— 掃描器必須用
    parse_float=str 讀，兩邊 sample 才是同一串。

    body 刻意手寫 bytes：走 `json.dumps` 的話 `1.10` 在進掃描器之前就已經
    被壓成 `1.1`，這條測試會拿一個自己弄壞的輸入去驗，然後因為錯的理由通過。
    """
    raw = b'{"tools":[{"input_schema":{"trace_id":1.10}}]}'
    findings = scan_request(raw)
    assert [f.sample for f in findings] == ["1.10"]


def test_number_literals_never_look_like_timestamp_or_uuid():
    """數字以字面值進來後仍會走過字串掃描；合法 JSON 數字字面值不可能
    長成 ISO-8601（要 T/:）或 UUID（要 4 個 `-` 分佈在 8/13/18/23）。"""
    findings = scan_request(_body({"system": [{"type": "text", "value": 1e10}]}))
    assert findings == []


# ─── 4. 不誤報 / 上限 / 非變性 ────────────────────────────────────────


def test_stable_content_yields_zero_findings():
    findings = scan_request(
        _body(
            {
                "system": "You are a helpful assistant. Be concise.",
                "messages": [
                    {"role": "user", "content": "Summarize the document below."},
                    {"role": "assistant", "content": "Sure — please paste it."},
                ],
                "tools": [
                    {
                        "name": "search",
                        "description": "Search the corpus.",
                        "input_schema": {
                            "type": "object",
                            "properties": {"query": {"type": "string"}},
                        },
                    }
                ],
            }
        )
    )
    assert findings == []


def test_caps_findings():
    messages = [
        {"role": "user", "content": f"turn {i}: 550e8400-e29b-41d4-a716-446655440000"}
        for i in range(30)
    ]
    assert len(scan_request(_body({"messages": messages}))) == MAX_FINDINGS


def test_sample_is_truncated_with_ellipsis():
    long_value = "x" * (SAMPLE_MAX_CHARS + 50)
    findings = scan_request(_body({"tools": [{"input_schema": {"session_id": long_value}}]}))
    assert findings[0].sample == "x" * SAMPLE_MAX_CHARS + "…"


def test_scan_does_not_mutate_input_bytes():
    """非變性不變量。入口吃 bytes、自己 parse 一份副本 ——
    掃描器手上根本沒有呼叫端的物件，改不到。"""
    raw = _body(
        {
            "system": "Today is 2026-05-04T14:30:00Z.",
            "messages": [{"role": "user", "content": "550e8400-e29b-41d4-a716-446655440000"}],
        }
    )
    before = bytes(raw)
    findings = scan_request(raw)
    assert findings, "這份 body 應該要掃出東西（否則本測試是空的比對）"
    assert raw == before


def test_malformed_input_returns_no_findings():
    """壞輸入原樣放行 —— M0 起一路貫穿的失敗模式契約。"""
    assert scan_request(b"not json at all") == []
    assert scan_request(b"[1,2,3]") == []
    assert scan_request(b"\xff\xfe") == []


# ─── 5. live zone 不掃（照抄解答本會踩的坑）──────────────────────────


def test_live_zone_volatile_content_is_not_reported():
    """最後一則訊息永遠在快取前綴之外 —— M3 的 `_place_breakpoints` 把
    標記 2 放在 `messages[-2]`，`messages[-1]` 從來就沒被快取過。

    那裡的時間戳每輪都變，而且變了無害。對它發警報就是純噪音，而使用者
    學會忽略觀測線之後，偵測器等於不存在。
    """
    findings = scan_request(
        _body(
            {
                "system": "You are a build assistant.",
                "messages": [
                    {"role": "user", "content": "hello"},
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "content": "2026-06-11T10:00:00 INFO ok\n2026-06-11T10:00:01 INFO ok",
                            }
                        ],
                    },
                ],
            }
        )
    )
    assert findings == []


def test_frozen_history_is_still_reported():
    """跳過的只有最後一則 —— 倒數第二則仍在前綴裡，仍要指出來。"""
    findings = scan_request(
        _body(
            {
                "messages": [
                    {"role": "user", "content": "started 2026-06-11T10:00:00Z"},
                    {"role": "assistant", "content": "ok"},
                ]
            }
        )
    )
    assert [(f.kind, f.location) for f in findings] == [(TIMESTAMP, "messages[0].content")]


def test_live_zone_noise_does_not_crowd_out_real_findings():
    """上限是全域的、走訪順序是 system → messages → tools。live zone 若
    能貢獻 findings，光它一則就能灌滿 10 筆，把 tools 裡真正該報的東西
    **安靜擠掉** —— 噪音之外還會漏報，這才是要修的理由。
    """
    noisy = "\n".join(f"2026-06-11T10:00:{i:02d} INFO tick" for i in range(40))
    findings = scan_request(
        _body(
            {
                "messages": [
                    {"role": "user", "content": "go"},
                    {"role": "user", "content": [{"type": "tool_result", "content": noisy}]},
                ],
                "tools": [
                    {
                        "name": "lookup",
                        "input_schema": {"properties": {"correlation_id": "ci-0417"}},
                    }
                ],
            }
        )
    )
    assert [(f.kind, f.location) for f in findings] == [
        (ID_FIELD, "tools[0].input_schema.properties.correlation_id")
    ]


def test_single_message_body_scans_nothing():
    """只有一則訊息時，那一則就是 live zone。"""
    findings = scan_request(
        _body({"messages": [{"role": "user", "content": "at 2026-06-11T10:00:00Z"}]})
    )
    assert findings == []


# ─── 6. 路徑（location）正確性 ─────────────────────────────────────────


def test_locations_for_block_lists():
    findings = scan_request(
        _body(
            {
                "system": [{"type": "text", "text": "now=2026-05-04T14:30:00Z"}],
                "messages": [
                    {
                        "role": "user",
                        "content": [{"type": "text", "text": "id=550e8400-e29b-41d4-a716-446655440000"}],
                    },
                    {"role": "user", "content": [{"type": "text", "text": "and now?"}]},
                ],
                "tools": [{"name": "t", "description": "since 2026-01-01T00:00:00Z"}],
            }
        )
    )
    assert [(f.kind, f.location) for f in findings] == [
        (TIMESTAMP, "system[0].text"),
        (UUID_V4, "messages[0].content[0].text"),
        (TIMESTAMP, "tools[0].description"),
    ]
