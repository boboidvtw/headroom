"""M0 安全網：證明 proxy 是 byte-faithful 的 passthrough。

對應 REALIGNMENT 計劃 PR-A1 / A3 / A8 的共同核心：
讀進來什麼 bytes，就原封不動送上游什麼 bytes。

測試手法（不碰真實 Anthropic）：
  - 上游：用 httpx.MockTransport 攔截，把它「實際收到的 bytes」錄下來。
  - proxy：用 httpx.ASGITransport 在 process 內驅動 FastAPI app。
  - 斷言：SHA-256(上游收到) == SHA-256(client 送出)。
"""

import hashlib

import httpx
import pytest

from headroom_lite.proxy import create_app


# 故意挑「json.dumps 重新序列化會破壞」的內容當試金石：
#   - 1.0  → 可能被寫成 1（float 掉精度）
#   - 大整數 → 可能變科學記號 / 掉精度
#   - 🔥 你好 → 可能變成 \uXXXX ASCII escape
TRICKY_BODY = (
    b'{"model":"claude-opus-4-8","temperature":1.0,'
    b'"seed":12345678901234567,'
    b'"messages":[{"role":"user","content":"\xf0\x9f\x94\xa5 \xe4\xbd\xa0\xe5\xa5\xbd"}]}'
)


@pytest.fixture
def captured_upstream():
    """回傳 (mock_client, captured) — captured['body'] 是上游實際收到的原始 bytes。"""
    captured: dict[str, bytes] = {}

    def handler(request: httpx.Request) -> httpx.Response:
        captured["body"] = request.content
        return httpx.Response(200, json={"ok": True})

    client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
    return client, captured


async def test_passthrough_body_is_byte_faithful(captured_upstream):
    upstream_client, captured = captured_upstream
    app = create_app(
        upstream_base_url="https://api.anthropic.com",
        client=upstream_client,
    )

    sent_hash = hashlib.sha256(TRICKY_BODY).hexdigest()

    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://proxy") as client:
        resp = await client.post(
            "/v1/messages",
            content=TRICKY_BODY,
            headers={"content-type": "application/json"},
        )

    assert resp.status_code == 200
    # 最關鍵的一行：上游收到的 bytes 必須與 client 送出的逐字節相同。
    assert captured["body"] == TRICKY_BODY
    assert hashlib.sha256(captured["body"]).hexdigest() == sent_hash


async def test_unicode_not_ascii_escaped(captured_upstream):
    """單獨盯死 unicode：上游收到的應是 UTF-8 原 bytes，不是 \\uXXXX。"""
    upstream_client, captured = captured_upstream
    app = create_app(upstream_base_url="https://api.anthropic.com", client=upstream_client)

    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://proxy") as client:
        await client.post("/v1/messages", content=TRICKY_BODY)

    assert b"\xf0\x9f\x94\xa5" in captured["body"]  # 🔥 的 UTF-8 bytes 還在
    assert rb"\ud83d" not in captured["body"]       # 沒有被 escape 成 \uXXXX
