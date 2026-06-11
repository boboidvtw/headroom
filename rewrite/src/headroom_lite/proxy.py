"""M0 — byte-faithful passthrough proxy（Phase A 之魂）。

這個 proxy 只做一件事，而且必須做到完美：
讀進 client 送來的原始 request bytes，原封不動轉發給上游。

為什麼這很重要？見 __init__.py 北極星 —— Anthropic prompt cache 靠
「逐字節前綴比對」命中。只要 proxy 重新序列化 body（json.dumps），
bytes 一變，cache 從那個 byte 起全 miss、費用暴增。

設計刻意用「工廠函式 + 注入 httpx client」：
  - 可測試性：測試時注入 MockTransport，攔截上游實際收到的 bytes。
  - 不可變：每次呼叫 create_app 回傳全新 app，不共享可變狀態。
"""

from __future__ import annotations

import httpx
from fastapi import FastAPI, Request, Response

# Hop-by-hop headers 不可轉發（RFC 7230 §6.1）；host / content-length
# 交給 httpx 依實際出站連線重算，硬轉發反而會對不上。
_HOP_BY_HOP = frozenset(
    {
        "host",
        "content-length",
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailers",
        "transfer-encoding",
        "upgrade",
    }
)


def _forward_request_headers(headers: httpx.Headers | dict) -> dict[str, str]:
    """挑出可安全轉發上游的 header。回傳新 dict（不可變風格）。"""
    return {
        key: value
        for key, value in headers.items()
        if key.lower() not in _HOP_BY_HOP
    }


def create_app(
    *,
    upstream_base_url: str,
    client: httpx.AsyncClient,
    transform=None,
) -> FastAPI:
    """建立一個把所有路徑 byte-faithful 轉發到 upstream_base_url 的 proxy app。

    Args:
        upstream_base_url: 上游基底，例如 "https://api.anthropic.com"。
        client: 已建好的 httpx.AsyncClient（測試時注入 MockTransport）。
        transform: 可選的 bytes -> bytes 轉換（例如 live_zone.compress_request）。
            None（預設）= 純 passthrough。引擎契約：不該動的輸入必須
            原樣回傳「原始 bytes 本人」，proxy 不替它把關。
    """
    upstream_base_url = upstream_base_url.rstrip("/")
    app = FastAPI()

    @app.api_route(
        "/{path:path}",
        methods=["GET", "POST", "PUT", "PATCH", "DELETE"],
    )
    async def passthrough(path: str, request: Request) -> Response:
        # (1) 取出「原始 bytes」——這是整個 proxy 的神聖輸入，絕不解析重序列化。
        raw_body = await request.body()
        if transform is not None and request.method == "POST":
            raw_body = transform(raw_body)

        url = f"{upstream_base_url}/{path}"
        if request.url.query:
            url = f"{url}?{request.url.query}"

        # (2) 關鍵的一行：content=raw_body（不是 json=...）。
        #     用 content 就是「把這串 bytes 照原樣放進 request body」。
        upstream_resp = await client.request(
            request.method,
            url,
            content=raw_body,
            headers=_forward_request_headers(request.headers),
        )

        # (3) 回程同理：把上游回的 bytes 原封不動交還 client。
        return Response(
            content=upstream_resp.content,
            status_code=upstream_resp.status_code,
            headers=_forward_request_headers(upstream_resp.headers),
            media_type=upstream_resp.headers.get("content-type"),
        )

    return app
