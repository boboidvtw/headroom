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
    MAX_LOCATION_CHARS,
    MAX_LOCATION_SEGMENT_CHARS,
    MAX_NESTING,
    MAX_SCAN_BYTES,
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
    assert findings.findings == []


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
    assert findings[0].sample == "550e8400…"


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
    assert id_fields[0].sample == "string[18]"


def test_id_field_with_empty_value_does_not_fire():
    """只『宣告』了 request_id 而沒填值的 schema 不該觸發。"""
    findings = scan_request(_body({"tools": [{"input_schema": {"properties": {"request_id": ""}}}]}))
    assert all(f.kind != ID_FIELD for f in findings)


def test_id_field_name_match_is_ascii_case_insensitive_substring():
    findings = scan_request(_body({"tools": [{"input_schema": {"X_Request_ID": 7}}]}))
    assert [f.kind for f in findings] == [ID_FIELD]
    assert findings[0].location == "tools[0].input_schema.X_Request_ID"


def test_numeric_id_value_is_never_rendered_as_a_literal():
    """數字一律描述成 `number`，不渲染字面值。

    這條測試的前身斷言「sample 保留原始字面值 `1.10`」，並宣稱
    `parse_float=str` 對齊了 Rust 的 `arbitrary_precision`。**那句話是錯的**
    —— 它只在「小數點後尾隨零」成立，而我只驗了 `1.10` 一個例子就推廣了。
    review 的差分 harness 打出來：`1E5` 在 Rust 會變 `1e+5`、`-0` 會變 `0`。
    三種都測，涵蓋參數空間兩側而不是只有當初碰巧驗過的那一點。
    """
    for raw in (
        b'{"tools":[{"input_schema":{"trace_id":1.10}}]}',
        b'{"tools":[{"input_schema":{"trace_id":1E5}}]}',
        b'{"tools":[{"input_schema":{"trace_id":-0}}]}',
    ):
        assert [f.sample for f in scan_request(raw)] == ["number"], raw


def test_number_literals_never_look_like_timestamp_or_uuid():
    """數字以字面值進來後仍會走過字串掃描；合法 JSON 數字字面值不可能
    長成 ISO-8601（要 T/:）或 UUID（要 4 個 `-` 分佈在 8/13/18/23）。"""
    findings = scan_request(_body({"system": [{"type": "text", "value": 1e10}]}))
    assert findings.findings == []


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
    assert findings.findings == []


def test_caps_distinct_locations_and_signals_truncation():
    messages = [
        {"role": "user", "content": f"turn {i}: 550e8400-e29b-41d4-a716-446655440000"}
        for i in range(30)
    ]
    scan = scan_request(_body({"messages": messages}))
    assert len(scan) == MAX_FINDINGS
    assert scan.truncated, "撞上限與『剛好 10 筆』不能長得一樣"


def test_repeated_hits_in_one_location_share_a_slot():
    """上限算的是**相異位置**不是命中次數。

    初版兩者不分，review 實測：三則含時間戳的凍結歷史就吃滿 10 個名額
    （只覆蓋 3 個相異位置），把 tools 裡唯一真正該報的欄位完全擠掉 ——
    噪音之外還會漏報。
    """
    noisy = " ".join(f"2026-06-11T10:00:{i:02d}" for i in range(40))
    scan = scan_request(
        _body(
            {
                "messages": [
                    {"role": "user", "content": noisy},
                    {"role": "user", "content": "live zone"},
                ],
                "tools": [{"input_schema": {"correlation_id": "ci-1"}}],
            }
        )
    )
    assert not scan.truncated
    assert [(f.kind, f.location, f.count) for f in scan] == [
        (TIMESTAMP, "messages[0].content", 40),
        (ID_FIELD, "tools[0].input_schema.correlation_id", 1),
    ]


def test_id_field_sample_never_contains_the_value():
    """sample 政策的核心守門。

    needle 是**子字串**比對，`session_identity_token` 命中 `session_id`
    —— 而 `session_id` 這種欄位在很多系統裡本身就是憑證。命中集合是開放的，
    列舉不完，所以唯一安全的作法是永遠不回吐值。
    """
    secret = "sk-ant-api03-REDACTEDSECRET-abcdefghij"
    findings = scan_request(
        _body({"tools": [{"input_schema": {"session_identity_token": secret}}]})
    )
    assert [f.kind for f in findings] == [ID_FIELD]
    assert findings[0].sample == f"string[{len(secret)}]"
    assert secret not in findings[0].sample


def test_id_field_sample_describes_type_and_size():
    cases = [
        ("abc", "string[3]"),
        (7, "number"),
        (True, "bool"),
        ([1, 2, 3], "array[3]"),
        ({"a": 1, "b": 2}, "object[2]"),
    ]
    for value, expected in cases:
        findings = scan_request(_body({"tools": [{"input_schema": {"trace_id": value}}]}))
        assert findings[0].sample == expected, value


def test_string_length_in_sample_counts_characters_not_bytes():
    """Python `len(str)` 是 code point，Rust 用 `chars().count()` ——
    用 byte 會讓非 ASCII 值的描述在兩邊分岔。"""
    findings = scan_request(_body({"tools": [{"input_schema": {"trace_id": "汉字漢"}}]}))
    assert findings[0].sample == "string[3]"


def test_uuid_sample_is_redacted_to_a_prefix():
    """v4 形狀的 API key 很常見；定位靠 location 就夠。"""
    findings = scan_request(
        _body(
            {
                "messages": [
                    {"role": "user", "content": "550e8400-e29b-41d4-a716-446655440000"},
                    {"role": "user", "content": "x"},
                ]
            }
        )
    )
    assert findings[0].sample == "550e8400…"


# ─── 4b. 輸入收斂：與 serde_json 的判準對齊 ──────────────────────────


def test_nan_is_rejected_whole_document():
    """Python 預設接受 NaN，serde_json 一律 Err —— 而且是**整包**失敗，
    所以拒收的粒度也必須是整份文件，不是那個欄位。"""
    assert scan_request(b'{"system":"2026-05-04T14:30:00Z","x":NaN}').findings == []


def test_non_utf8_and_bom_are_rejected():
    """`json.loads(bytes)` 會依 BOM / null byte 自動偵測 UTF-16/32，
    `serde_json::from_slice` 只吃 UTF-8。"""
    assert scan_request('{"system":"2026-05-04T14:30:00Z"}'.encode("utf-16-le")).findings == []
    assert scan_request(b'\xef\xbb\xbf{"system":"2026-05-04T14:30:00Z"}').findings == []


def test_lone_surrogate_rejected_but_paired_still_works():
    """守門要同時測「該擋的擋了」與「該過的還會過」—— 成對 surrogate
    （emoji）是合法 JSON，不可以被一起擋掉。"""
    assert scan_request(b'{"system":"\\ud800 2026-05-04T14:30:00Z"}').findings == []
    paired = scan_request(b'{"system":"\\ud83d\\ude00 2026-05-04T14:30:00Z"}')
    assert [f.kind for f in paired] == [TIMESTAMP]


def test_deep_nesting_boundary_matches_serde_json():
    """serde_json 的 parse 深度上限是 128 層容器。**兩側都釘** ——
    只釘一側的話另一側漂了不會有人知道。"""
    def nest(depth: int) -> bytes:
        return ('{"system":' + "[" * depth + '"2026-05-04T14:30:00Z"' + "]" * depth + "}").encode()

    assert len(scan_request(nest(MAX_NESTING - 1))) == 1
    assert scan_request(nest(MAX_NESTING)).findings == []


def test_deep_nesting_does_not_raise():
    """初版在這裡拋 RecursionError，直接違反「絕不拋例外」的契約 ——
    而且 `except (ValueError, ...)` 接不住（RecursionError 不是 ValueError）。"""
    deep = ('{"system":' + "[" * 5000 + '"2026-05-04T14:30:00Z"' + "]" * 5000 + "}").encode()
    assert scan_request(deep).findings == []


def test_oversized_body_is_skipped_with_its_own_signal():
    """觀測是盡力而為的功能。這條路徑跑在轉發之前，掃多久就是延遲多久。

    **訊號不可與撞上限共用**：初版兩者共用 `truncated`，於是 proxy 會對一份
    根本沒掃過的 body 印出「已達 10 個相異位置的上限」—— 修掉了第一層歧義，
    又在同一個欄位上長出第二層。
    """
    huge = b'{"system":"' + b"x" * (MAX_SCAN_BYTES + 1) + b'"}'
    scan = scan_request(huge)
    assert scan.findings == []
    assert scan.skipped_too_large, "放棄掃描必須留下訊號"
    assert not scan.truncated, "沒掃過的 body 不該宣稱『撞到 findings 上限』"


def test_rejected_inputs_set_no_signal_at_all():
    """拒收路徑不得誤設任何旗標 —— 否則下游會把『看不懂』當成『還有更多』。"""
    for raw in (b"not json", b'{"system":"2026-05-04T14:30:00Z","x":NaN}', b"\xff\xfe"):
        scan = scan_request(raw)
        assert scan.findings == []
        assert not scan.truncated and not scan.skipped_too_large, raw


# ─── 4c. location 設界（review 二輪）──────────────────────────────────


def test_location_segment_is_capped():
    """location 是**客戶 key 名**串起來的路徑，祖先 key 完全不受 needle 約束。
    一份用 email 或 token 當 map key 的 JSON，整串都會進觀測線。"""
    long_key = "A" * 200
    findings = scan_request(
        _body({"tools": [{"input_schema": {long_key: {"trace_id": "v"}}}]})
    )
    assert findings[0].location == (
        f"tools[0].input_schema.{'A' * MAX_LOCATION_SEGMENT_CHARS}….trace_id"
    )


def test_location_total_length_is_capped():
    """沒有總長上限的話，200 KB 的 key 名就是 200 KB 的單行 stderr。"""
    nested = {"trace_id": "v"}
    for i in range(40):
        nested = {f"segment_{i}_{'x' * 30}": nested}
    findings = scan_request(_body({"tools": [{"input_schema": nested}]}))
    assert len(findings[0].location) <= MAX_LOCATION_CHARS + 1  # +1 為省略號
    assert findings[0].location.endswith(".trace_id"), "尾端命中欄位必須保留"
    assert findings[0].location.startswith("tools[0]"), "頭部定位資訊必須保留"


def test_deep_value_via_public_api_does_not_blow_up():
    """`detect_volatile_content` 是公開 API、可直接收手工建的結構。
    Rust 那側在這種情況會 stack overflow 而 **abort**（連攔都攔不到），
    所以兩邊都在走訪內再擋一次深度。"""
    from headroom_lite.volatile import detect_volatile_content

    deep = {"trace_id": "2026-05-04T14:30:00Z"}
    for _ in range(5000):
        deep = {"nest": deep}
    scan = detect_volatile_content({"system": deep})
    assert scan.findings == []


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
    assert scan_request(b"not json at all").findings == []
    assert scan_request(b"[1,2,3]").findings == []
    assert scan_request(b"\xff\xfe").findings == []


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
    assert findings.findings == []


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
    assert findings.findings == []


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
