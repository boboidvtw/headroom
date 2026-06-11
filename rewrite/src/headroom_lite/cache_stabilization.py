"""M3 — cache 穩定化（Phase E 之魂）。

從「不搞砸 cache」進階到「主動幫 client 提高命中率」：

  1. tools 正規化（E1/E2）—— tools 在 cache 前綴最前面，client 的
     tool 順序漂移會讓整條前綴從第 0 byte 開始 miss。proxy 套一個
     「確定性正規化」（按名字排序 + schema key 遞迴排序），不管
     client 怎麼漂，出口都收斂成同一份 bytes。
     ※ 正規化會改 client 的 bytes —— 被允許的原因和 M1 相同：
       它是確定性函式，跨輪穩定，上游看到的前綴反而更穩。

  2. cache_control 自動放置（E3）—— Anthropic 的 cache 要 client
     主動放標記（上限 4 個）才會生效，很多 client 忘了放。
     紅線：client 已放任何標記 = 客戶的明確意圖，一個都不准動、
     也不准畫蛇添足；只有「全身零標記」才出手補。

老規矩（M0 起一路貫穿）：沒事可做回傳「原始 bytes 本人」、
壞輸入原樣放行、整個轉換必須確定性。
"""

from __future__ import annotations

import json

_EPHEMERAL = {"type": "ephemeral"}


def _sort_keys_recursively(obj):
    """遞迴排序所有 dict 的 key，回傳全新結構（不可變風格）。"""
    if isinstance(obj, dict):
        return {k: _sort_keys_recursively(obj[k]) for k in sorted(obj)}
    if isinstance(obj, list):
        return [_sort_keys_recursively(v) for v in obj]
    return obj


def _normalize_tools(tools: list) -> list:
    """tools 按名字排序；input_schema 的 key 遞迴排序。

    只動 input_schema 內部 —— tool 自身的 name/description 等
    top-level key 順序保留 client 原樣（沒必要動的不動）。
    """
    normalized = [
        {**t, "input_schema": _sort_keys_recursively(t["input_schema"])}
        if isinstance(t, dict) and isinstance(t.get("input_schema"), dict)
        else t
        for t in tools
    ]
    return sorted(
        normalized,
        key=lambda t: t.get("name", "") if isinstance(t, dict) else "",
    )


def _iter_blocks(body: dict):
    """走訪所有可能帶 cache_control 的 content block。"""
    system = body.get("system")
    if isinstance(system, list):
        yield from (b for b in system if isinstance(b, dict))
    for message in body.get("messages") or []:
        content = message.get("content") if isinstance(message, dict) else None
        if isinstance(content, list):
            yield from (b for b in content if isinstance(b, dict))
    for tool in body.get("tools") or []:
        if isinstance(tool, dict):
            yield tool


def _has_any_marker(body: dict) -> bool:
    return any("cache_control" in block for block in _iter_blocks(body))


def _mark_last_block(blocks: list) -> list | None:
    """把標記放在 block list 的最後一個 dict block 上。回傳新 list；
    沒有可放的位置回傳 None。"""
    for i in range(len(blocks) - 1, -1, -1):
        if isinstance(blocks[i], dict):
            marked = {**blocks[i], "cache_control": _EPHEMERAL}
            return [*blocks[:i], marked, *blocks[i + 1:]]
    return None


def _place_breakpoints(body: dict) -> dict:
    """零標記時自動補（學習版放 2 個，上限 4）：

      標記 1：system 最後一個 block —— 涵蓋 tools + system 整段前綴。
      標記 2：live zone 前的最後一則訊息 —— 涵蓋整段對話歷史。
      live zone 本身不放：它下一輪就變了，快取了也命中不到。
    """
    new_body = dict(body)

    system = new_body.get("system")
    if isinstance(system, list):
        marked = _mark_last_block(system)
        if marked is not None:
            new_body["system"] = marked

    messages = new_body.get("messages")
    if isinstance(messages, list) and len(messages) >= 2:
        frozen_last = messages[-2]
        content = frozen_last.get("content") if isinstance(frozen_last, dict) else None
        if isinstance(content, list):
            marked = _mark_last_block(content)
            if marked is not None:
                new_body["messages"] = [
                    *messages[:-2],
                    {**frozen_last, "content": marked},
                    messages[-1],
                ]
    return new_body


def stabilize_request(raw: bytes) -> bytes:
    """入口：對 body bytes 做 cache 穩定化。

    只在「真的改到東西」時才重新序列化；否則回傳原始 bytes 本人。
    """
    try:
        body = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        return raw
    if not isinstance(body, dict):
        return raw

    new_body = dict(body)
    changed = False

    tools = body.get("tools")
    if isinstance(tools, list) and tools:
        normalized = _normalize_tools(tools)
        # 變更偵測必須用「序列化後的 bytes」比，不能用 ==：
        # Python 的 dict == 不在乎 key 順序，但 cache 前綴在乎
        # —— M0 的教訓（語意相等 ≠ bytes 相等）在這裡再現。
        if json.dumps(normalized) != json.dumps(tools):
            new_body["tools"] = normalized
            changed = True

    if not _has_any_marker(body):
        placed = _place_breakpoints(new_body)
        if placed != new_body:
            new_body = placed
            changed = True

    if not changed:
        return raw
    return json.dumps(new_body, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
