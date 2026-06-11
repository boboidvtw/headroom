"""M4 — CCR 可逆取回（Compress-Cache-Retrieve）。

這專案相對其他壓縮 proxy 的差異化招牌：**壓縮永不丟資料**。

  - 原文搬進 content-addressed store，key = 內容的 sha256 前 16 碼
    —— 正是 M1 壓縮標記裡那個 hash。模型在對話裡看得到 key，
    需要原文時呼叫 ccr_retrieve 工具取回。
  - content-addressed 的妙處：key 由內容決定，同文必同 key，
    store 天然去重、不需要任何協調或序號。

鐵律 4（計劃 B7）：ccr_retrieve「每請求都註冊」—— 包括這輪
什麼都沒壓的請求。tools 陣列在 cache 前綴最前面，工具時有時無
= 前綴閃爍 = cache 炸（原版的 bug 正是如此）。
"""

from __future__ import annotations

import hashlib
import json

KEY_HEX_LEN = 16


def content_key(text: str) -> str:
    """內容定址 key：sha256 前 16 碼。live_zone 的標記與 store 共用。"""
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:KEY_HEX_LEN]


class CCRStore:
    """content-addressed 原文倉庫（學習版：記憶體 dict）。

    正式版會是持久化 backend（計劃 B7）；介面刻意只有 put/get，
    換 backend 不動呼叫端。
    """

    def __init__(self) -> None:
        self._items: dict[str, str] = {}

    def put(self, text: str) -> str:
        """存入原文，回傳 key。同文同 key —— 天然去重。"""
        key = content_key(text)
        self._items[key] = text
        return key

    def get(self, key: str) -> str | None:
        return self._items.get(key)

    def __len__(self) -> int:
        return len(self._items)


# 工具定義是「凍結」的：dict 內容與 key 順序永不變動。
# 跨輪 bytes 必須逐字節相同 —— 差一個 byte，tools 前綴就炸。
CCR_RETRIEVE_TOOL = {
    "name": "ccr_retrieve",
    "description": (
        "取回先前被壓縮省略的完整原文。對話中形如 "
        "[... headroom-lite squeezed N lines | sha256:KEY ...] 的標記，"
        "代表該處原文已存放於側信道，可用 KEY 取回。"
    ),
    "input_schema": {
        "properties": {
            "key": {
                "description": "標記中 sha256: 後的 16 碼 hex key",
                "type": "string",
            }
        },
        "required": ["key"],
        "type": "object",
    },
}


def register_ccr_tool(raw: bytes) -> bytes:
    """把 ccr_retrieve 註冊進 body 的 tools 陣列 —— 無條件、每請求。

    已存在（冪等）或壞輸入 → 回傳原始 bytes 本人。
    """
    try:
        body = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        return raw
    if not isinstance(body, dict):
        return raw

    tools = body.get("tools")
    if not isinstance(tools, list):
        tools = []
    if any(isinstance(t, dict) and t.get("name") == "ccr_retrieve" for t in tools):
        return raw  # 冪等：已註冊就一個 byte 都不動

    new_body = {**body, "tools": [*tools, CCR_RETRIEVE_TOOL]}
    return json.dumps(new_body, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def handle_retrieve(store: CCRStore, key: str) -> str:
    """處理模型的 ccr_retrieve 呼叫：回原文，或誠實說找不到。"""
    original = store.get(key)
    if original is None:
        return f"[ccr_retrieve] 找不到 key={key} 的內容（可能已過期或 key 有誤）"
    return original
