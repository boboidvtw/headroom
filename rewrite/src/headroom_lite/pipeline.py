"""M8 — headroom-lite pipeline orchestration（lazy registration 之魂）。

把三段引擎串成完整流程，並決定 ccr_retrieve 的「註冊時機」。

歷史教訓（2026-06-12 live traffic 實測）
----------------------------------------
原設計每請求都先 register_ccr_tool，無條件在 tools 陣列（cache 前綴
最前面）加一個工具。實測證明：這會害上游對 raw 流量的 ~30k 部分命中
容錯失效 —— 多數請求根本沒壓縮，卻全都付了 tools 前綴變動的代價。

M8 治本：lazy registration
--------------------------
新順序 `stabilize → compress →（有壓到才 register）`：
  - register_ccr_tool 維持「無條件」的純 building block；
    「何時呼叫它」的決策上移到這層。
  - 訊號：compress 沒壓到任何東西時回傳「原始 bytes 本人」
    （identity），據此判斷這輪要不要註冊。
  - 多數 session 不壓縮 → tools 全程不動 → 零 cache 影響。
  - 真壓到了才註冊；ccr_retrieve 接在 tools 尾端、不重排 ——
    prefix cache 逐字節前綴比對，擺尾端讓 client 既有 tools 維持
    byte-identical，divergence point 往後推、保住更多前綴。

失敗模式契約穿透整條 pipeline：任一段拿不出可用結果，
最終都回傳「原始 bytes 本人」。
"""

from __future__ import annotations

from headroom_lite.cache_stabilization import stabilize_request
from headroom_lite.ccr import register_ccr_tool
from headroom_lite.live_zone import compress_request


def process_request(raw: bytes, *, store=None) -> bytes:
    """對 /v1/messages 的 body bytes 跑完整 headroom-lite pipeline。

    Args:
        raw: client 送來的原始 request body bytes。
        store: 可選的 CCRStore —— 給了就在壓縮前收存原文，可逆取回（M4）。

    Returns:
        處理後的 bytes；沒事可做時回傳「原始 bytes 本人」（同一物件）。
    """
    stabilized = stabilize_request(raw)
    compressed = compress_request(stabilized, store=store)

    # compress 沒壓到任何東西時回傳「傳進去的同一物件」（identity）。
    # 據此判斷這輪要不要註冊 —— stabilize 單獨動過手不算「壓縮」，
    # 不該觸發 ccr 註冊（lazy 的精髓）。
    if compressed is stabilized:
        return compressed

    # 有壓到 → 註冊 ccr_retrieve，讓被省略的原文可被追問取回。
    # 接在（已排序的 client）tools 尾端，cache 前綴保留最大化。
    return register_ccr_tool(compressed)
