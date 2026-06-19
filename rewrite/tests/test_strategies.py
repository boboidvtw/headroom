"""M11 安全網：壓縮策略 dispatcher。

骨架階段只有一個策略（truncate，catch-all）。這些測試鎖住的不變式：
  1. dispatcher 把活派給「第一個 applies 命中」的策略，命中即停。
  2. truncate 是永遠適用的 catch-all（殿後保底，沒有內容感知策略接手時兜住）。
  3. store.put 時機契約：門檻沒過（行數太少）絕不 put —— parity 逐字節依賴此時機。
  4. 確定性 + CCR 標記含 content_key（同一份原文永遠對到同一個 key）。

重構不變式：squeeze_text 對長 tool_result 的輸出，必須與 M1 舊截斷行為逐字一致。
"""

from headroom_lite.ccr import content_key
from headroom_lite.strategies import (
    TRUNCATE,
    Strategy,
    squeeze_text,
)

# 一段夠長、保證跨過截斷門檻（> HEAD+TAIL 行）的文字。
LONG_TEXT = "\n".join(f"line {i}" for i in range(100))
# 行數太少：truncate 門檻不過 —— 不壓、不 put。
SHORT_TEXT = "\n".join(f"line {i}" for i in range(5))


class _SpyStore:
    """記錄 put 呼叫次數的測試替身。"""

    def __init__(self) -> None:
        self.puts: list[str] = []

    def put(self, text: str) -> str:
        self.puts.append(text)
        return content_key(text)


def test_truncate_applies_is_catch_all():
    # catch-all：對任何輸入都回 True，永遠能兜底。
    assert TRUNCATE.applies(LONG_TEXT) is True
    assert TRUNCATE.applies(SHORT_TEXT) is True
    assert TRUNCATE.applies("") is True


def test_dispatcher_routes_to_first_matching_strategy():
    # 前置一個永遠命中的 dummy 策略 → 它該贏，truncate 不被呼叫。
    sentinel = "DUMMY-WON"
    dummy = Strategy(
        name="dummy",
        applies=lambda text: True,
        squeeze=lambda text, store=None: sentinel,
    )
    out = squeeze_text(LONG_TEXT, strategies=(dummy, TRUNCATE))
    assert out == sentinel


def test_dispatcher_skips_non_matching_strategy():
    # 第一個策略 sniff 不命中 → 跳過，落到 truncate。
    never = Strategy(
        name="never",
        applies=lambda text: False,
        squeeze=lambda text, store=None: "SHOULD-NOT-RUN",
    )
    out = squeeze_text(LONG_TEXT, strategies=(never, TRUNCATE))
    assert out != "SHOULD-NOT-RUN"
    assert "headroom-lite squeezed" in out  # 是 truncate 的標記


def test_no_strategy_matches_returns_original():
    # 防禦性：沒有任何策略命中（移除 catch-all）→ 原文原樣回。
    never = Strategy("never", lambda t: False, lambda t, store=None: "X")
    assert squeeze_text(LONG_TEXT, strategies=(never,)) == LONG_TEXT


def test_truncate_marker_contains_content_key():
    out = squeeze_text(LONG_TEXT)
    assert f"sha256:{content_key(LONG_TEXT)}" in out
    assert "headroom-lite squeezed" in out


def test_truncate_stores_original_before_squeezing():
    store = _SpyStore()
    squeeze_text(LONG_TEXT, store=store)
    assert store.puts == [LONG_TEXT]  # 壓縮前收存原文一次


def test_store_put_timing_skipped_when_below_threshold():
    # 神聖 spec：行數不過門檻 → 回原文、絕不 put（M6 教訓）。
    store = _SpyStore()
    out = squeeze_text(SHORT_TEXT, store=store)
    assert out == SHORT_TEXT
    assert store.puts == []


def test_deterministic_same_input_same_output():
    assert squeeze_text(LONG_TEXT) == squeeze_text(LONG_TEXT)


# ── 加嚴：門檻邊界 + unicode（與 Rust tests/strategies.rs 對稱）──
def test_exactly_at_threshold_not_compressed():
    from headroom_lite.strategies import HEAD_LINES, TAIL_LINES
    n = HEAD_LINES + TAIL_LINES
    text = "\n".join(f"l{i}" for i in range(n))
    assert squeeze_text(text) == text  # 剛好門檻不壓，回原文


def test_one_over_threshold_is_compressed():
    from headroom_lite.strategies import HEAD_LINES, TAIL_LINES
    n = HEAD_LINES + TAIL_LINES + 1
    text = "\n".join(f"l{i}" for i in range(n))
    assert "squeezed 1 lines" in squeeze_text(text)


def test_unicode_text_deterministic_and_keyed():
    line = "中文行 \U0001f338 emoji"
    text = "\n".join(f"{line} {i}" for i in range(50))
    assert squeeze_text(text) == squeeze_text(text)
    assert f"sha256:{content_key(text)}" in squeeze_text(text)
