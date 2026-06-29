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


# ── M12：log 內容感知策略（與 Rust tests/strategies.rs 對稱）──
from headroom_lite.strategies import LOG, STRATEGIES, _log_squeeze, _severity


def _noisy_log(n_errors: int = 5, n_noise: int = 20) -> str:
    """一份噪音夾雜的 log：error 刻意埋在「中段」（truncate 的盲區）。"""
    half = n_noise // 2
    lines = [f"2026-06-20 10:00:{i:02d} DEBUG worker tick {i}" for i in range(half)]
    lines += [f"2026-06-20 10:01:{i:02d} ERROR db connection failed attempt {i}" for i in range(n_errors)]
    lines += [f"2026-06-20 10:02:{i:02d} INFO retrying job {i}" for i in range(n_noise - half)]
    return "\n".join(lines)


def test_log_applies_on_noisy_log():
    # 噪音佔比高、可分類行多 → log 策略認領。
    assert LOG.applies(_noisy_log()) is True


def test_log_applies_false_on_prose():
    prose = "\n".join(f"This is sentence number {i} about nothing in particular." for i in range(30))
    assert LOG.applies(prose) is False


def test_log_applies_false_when_no_noise():
    # 全 ERROR：沒噪音可丟 → 不認領，交給 truncate 兜底。
    all_err = "\n".join(f"ERROR something broke {i}" for i in range(30))
    assert LOG.applies(all_err) is False


def test_log_squeeze_drops_noise_keeps_errors():
    out = _log_squeeze(_noisy_log())
    assert "DEBUG" not in out
    assert "INFO" not in out
    assert out.count("ERROR") == 5  # 高嚴重度全留


def test_log_keeps_middle_errors_unlike_truncate():
    # 3 個 error 埋在 60 行噪音中段；truncate 頭尾保留會丟掉它們，log 全留。
    log = _noisy_log(n_errors=3, n_noise=60)
    out = squeeze_text(log)
    assert out.count("ERROR") == 3
    assert "dropped" in out  # 走的是 log 策略，不是 truncate 的 "squeezed"


def test_log_marker_has_count_and_key():
    log = _noisy_log()
    out = squeeze_text(log)
    assert f"sha256:{content_key(log)}" in out
    assert "dropped 20 log lines" in out


def test_log_deterministic():
    log = _noisy_log()
    assert squeeze_text(log) == squeeze_text(log)


def test_log_stores_original_before_squeezing():
    store = _SpyStore()
    log = _noisy_log()
    squeeze_text(log, store=store)
    assert store.puts == [log]  # 產出有損輸出前收存原文一次


def test_log_no_drop_returns_text_without_put():
    # 防禦性：squeeze 直呼但無噪音可丟 → 原文回、絕不 put（守神聖收存時機）。
    store = _SpyStore()
    all_err = "\n".join(f"ERROR x {i}" for i in range(10))
    assert _log_squeeze(all_err, store) == all_err
    assert store.puts == []


def test_log_registered_before_truncate():
    names = [s.name for s in STRATEGIES]
    assert names.index("log") < names.index("truncate")


def test_word_boundary_information_not_info():
    # 整詞比對：INFORMATION 不該被當成 INFO。
    assert _severity("2026 INFORMATION about the system") == "other"
    assert _severity("2026 INFO about the system") == "drop"
    assert _severity("2026 WARNING disk almost full") == "keep"


# ── M13：diff 內容感知策略（與 Rust tests/strategies.rs 對稱）──
from headroom_lite.strategies import DIFF, _diff_squeeze


def _diff(n_context: int = 20, n_changes: int = 4) -> str:
    """一份 context 夾雜的 unified diff：變更刻意埋在「中段」（truncate 的盲區）。"""
    half = n_context // 2
    lines = ["diff --git a/app.py b/app.py", "index 1111111..2222222 100644",
             "--- a/app.py", "+++ b/app.py", "@@ -1,40 +1,40 @@ def main():"]
    lines += [f" context line {i} unchanged" for i in range(half)]
    lines += [f"-old line {i}" for i in range(n_changes)]
    lines += [f"+new line {i}" for i in range(n_changes)]
    lines += [f" context line {i} unchanged" for i in range(half, n_context)]
    return "\n".join(lines)


def test_diff_applies_on_unified_diff():
    # 有 hunk header、context 佔比高 → diff 策略認領。
    assert DIFF.applies(_diff()) is True


def test_diff_applies_false_without_hunk_header():
    # 像 diff 的 +/- 但沒 hunk header（如 markdown 條列）→ 不認領。
    no_hunk = "\n".join([f"- bullet {i}" if i % 2 else f"+ bullet {i}" for i in range(30)])
    assert DIFF.applies(no_hunk) is False


def test_diff_applies_false_when_no_context():
    # 全變更行、無 context 可丟 → 不認領，交給後手兜底。
    all_changes = "@@ -1,5 +1,5 @@\n" + "\n".join(f"+line {i}" for i in range(30))
    assert DIFF.applies(all_changes) is False


def test_diff_squeeze_drops_context_keeps_changes():
    out = _diff_squeeze(_diff())
    assert "unchanged" not in out  # 未變更的 context 全丟（標記文字含 "context" 故查內容字串）
    assert out.count("-old line") == 4  # 所有移除行保留
    assert out.count("+new line") == 4  # 所有新增行保留
    assert "@@ -1,40 +1,40 @@" in out  # hunk header 保留
    assert "diff --git a/app.py b/app.py" in out  # 檔頭保留


def test_diff_keeps_middle_changes_unlike_truncate():
    # 變更埋在大段 context 中段；truncate 頭尾保留會丟掉它們，diff 全留。
    diff = _diff(n_context=60, n_changes=3)
    out = squeeze_text(diff)
    assert out.count("-old line") == 3
    assert out.count("+new line") == 3
    assert "diff context lines" in out  # 走 diff 策略，不是 truncate 的 "squeezed"


def test_diff_marker_has_count_and_key():
    diff = _diff()
    out = squeeze_text(diff)
    assert f"sha256:{content_key(diff)}" in out
    assert "dropped 20 diff context lines" in out


def test_diff_deterministic():
    diff = _diff()
    assert squeeze_text(diff) == squeeze_text(diff)


def test_diff_stores_original_before_squeezing():
    store = _SpyStore()
    diff = _diff()
    squeeze_text(diff, store=store)
    assert store.puts == [diff]  # 產出有損輸出前收存原文一次


def test_diff_no_drop_returns_text_without_put():
    # 防禦性：squeeze 直呼但無 context 可丟 → 原文回、絕不 put（守神聖收存時機）。
    store = _SpyStore()
    all_changes = "@@ -1,5 +1,5 @@\n" + "\n".join(f"+line {i}" for i in range(10))
    assert _diff_squeeze(all_changes, store) == all_changes
    assert store.puts == []


def test_diff_registered_before_log_and_truncate():
    names = [s.name for s in STRATEGIES]
    assert names.index("diff") < names.index("log") < names.index("truncate")


# ── M14：search 內容感知策略（與 Rust tests/strategies.rs 對稱）──
from headroom_lite.strategies import SEARCH, _match_line_key, _search_squeeze


def _search(n_files: int = 3, per_file: int = 12) -> str:
    """grep/rg 風格輸出：每檔多筆命中（超過 KEEP_PER_FILE → 可丟）。"""
    lines = []
    for f in range(n_files):
        for ln in range(per_file):
            lines.append(f"./src/module_{f}.py:{ln + 1}:    result = compute(value_{ln})")
    return "\n".join(lines)


def test_search_match_line_requires_slash_in_path():
    # 真 grep 行（含路徑）→ match。
    ok, key = _match_line_key("./src/foo.py:42:hit")
    assert ok is True and key == "./src/foo.py"


def test_search_match_line_rejects_timestamp():
    # ⚠️ 地雷防線：log 時間戳 `10:30:45` 不含 `/` → 不可被當成 match line。
    assert _match_line_key("2026-06-20T10:30:45 ERROR boom")[0] is False
    assert _match_line_key("10:30:45 something")[0] is False


def test_search_applies_on_grep_output():
    # 每檔 12 筆、保 3 丟 9 → 可丟佔比高，search 認領。
    assert SEARCH.applies(_search()) is True


def test_search_applies_false_on_prose():
    prose = "\n".join(f"This is sentence number {i} about nothing." for i in range(30))
    assert SEARCH.applies(prose) is False


def test_search_applies_false_when_under_cap():
    # 每檔只 2 筆（≤ KEEP_PER_FILE）→ 無可丟，不認領。
    text = _search(n_files=5, per_file=2)
    assert SEARCH.applies(text) is False


def test_search_squeeze_caps_per_file():
    from headroom_lite.strategies import KEEP_PER_FILE
    out = _search_squeeze(_search(n_files=3, per_file=12))
    # 每檔保留恰好 KEEP_PER_FILE 筆。
    for f in range(3):
        assert out.count(f"./src/module_{f}.py:") == KEEP_PER_FILE
    assert "dropped" in out


def test_search_marker_has_count_and_key():
    text = _search(n_files=3, per_file=12)  # 每檔丟 9 → 共丟 27
    out = squeeze_text(text)
    assert f"sha256:{content_key(text)}" in out
    assert "dropped 27 search result lines" in out


def test_search_deterministic():
    text = _search()
    assert squeeze_text(text) == squeeze_text(text)


def test_search_stores_original_before_squeezing():
    store = _SpyStore()
    text = _search()
    squeeze_text(text, store=store)
    assert store.puts == [text]


def test_search_no_drop_returns_text_without_put():
    # 防禦性：squeeze 直呼但無超量可丟 → 原文回、絕不 put。
    store = _SpyStore()
    text = _search(n_files=4, per_file=2)
    assert _search_squeeze(text, store) == text
    assert store.puts == []


def test_search_does_not_swallow_logs():
    # 關鍵不回歸：噪音 log 仍走 log（含時間戳但無 /+數字 match 行）→ search 不吃。
    log = _noisy_log()
    assert SEARCH.applies(log) is False
    assert "log lines" in squeeze_text(log)  # 仍是 log 策略的標記


def test_search_registered_after_diff_before_log():
    names = [s.name for s in STRATEGIES]
    assert names.index("diff") < names.index("search") < names.index("log") < names.index("truncate")


# ── M15：json 內容感知策略（與 Rust tests/strategies.rs 對稱）──
import json as _json
from headroom_lite.strategies import JSON, _json_squeeze, _json_squeeze_core, _starts_json


def _json_array(n: int = 20) -> str:
    """同質物件的大型 JSON array（compact，模擬 API 回應）。"""
    return _json.dumps([{"id": i, "name": f"item_{i}", "active": i % 2 == 0} for i in range(n)],
                       separators=(",", ":"))


def test_json_starts_detector():
    assert _starts_json('  \n  [1,2,3]') is True
    assert _starts_json('\t{"a":1}') is True
    assert _starts_json('hello [1,2,3]') is False  # 含括號但非 JSON 文件


def test_json_applies_on_large_array():
    assert JSON.applies(_json_array(20)) is True


def test_json_applies_false_on_small_array():
    # 元素不足 11（HEAD5+TAIL2+DROP4）→ 不認領。
    assert JSON.applies(_json_array(8)) is False


def test_json_applies_false_on_prose():
    prose = "\n".join(f"sentence {i} with [brackets] and, commas" for i in range(30))
    assert JSON.applies(prose) is False


def test_json_squeeze_keeps_head_tail_and_valid_json():
    from headroom_lite.strategies import JSON_HEAD, JSON_TAIL
    out = _json_squeeze_core(_json_array(20))
    parsed = _json.loads(out)  # 結果必須是合法 JSON
    assert len(parsed) == JSON_HEAD + JSON_TAIL + 1  # 頭 + marker + 尾
    assert parsed[0] == {"id": 0, "name": "item_0", "active": True}  # 頭元素原文保留
    assert parsed[-1] == {"id": 19, "name": "item_19", "active": False}  # 尾元素保留
    assert "dropped 13 array elements" in parsed[JSON_HEAD]  # 中間是 marker 字串


def test_json_never_reserializes_numbers():
    # ⭐ parity 正解驗證：1.10 這類數字照抄原文、不被正規化成 1.1。
    text = '[' + ",".join(['{"v":1.10}'] * 20) + ']'
    out = _json_squeeze_core(text)
    assert "1.10" in out  # 原文保留
    assert "1.1," not in out.replace("1.10", "")  # 沒有被正規化掉尾零


def test_json_nested_picks_largest_array():
    # 物件內含大 array → 找到並截斷它、外層結構照抄。
    text = '{"meta":{"n":3},"data":' + _json_array(20) + ',"ok":true}'
    out = _json_squeeze_core(text)
    assert out.startswith('{"meta":{"n":3},"data":[')
    assert out.endswith(',"ok":true}')
    assert "dropped 13 array elements" in out


def test_json_marker_has_count_and_key():
    text = _json_array(20)
    out = squeeze_text(text)
    assert f"sha256:{content_key(text)}" in out
    assert "dropped 13 array elements" in out


def test_json_deterministic():
    text = _json_array(20)
    assert squeeze_text(text) == squeeze_text(text)


def test_json_stores_original_before_squeezing():
    store = _SpyStore()
    text = _json_array(20)
    squeeze_text(text, store=store)
    assert store.puts == [text]


# ── M16：stack trace 內容感知策略（與 Rust tests/strategies.rs 對稱）──
from headroom_lite.strategies import (
    STACKTRACE,
    _is_frame_header,
    _stacktrace_squeeze,
)


def _py_recursion_trace(frames: int = 15) -> str:
    """Python RecursionError traceback：N 個逐字相同的 2 行 frame（典型遞迴爆炸）。"""
    head = "Traceback (most recent call last):"
    body = "\n".join(
        '  File "/app/rec.py", line 3, in foo\n    return foo(n - 1)'
        for _ in range(frames)
    )
    tail = "RecursionError: maximum recursion depth exceeded"
    return f"{head}\n{body}\n{tail}"


def _java_trace(frames: int = 15) -> str:
    """Java 風格單行 frame：`\\tat pkg.Class.method(File.java:line)`。"""
    head = "Exception in thread \"main\" java.lang.StackOverflowError"
    body = "\n".join(f"\tat com.example.App.foo(App.java:{10 + i})" for i in range(frames))
    return f"{head}\n{body}"


def test_stack_is_frame_header_python_and_java():
    assert _is_frame_header('  File "/app/x.py", line 3, in foo') is True
    assert _is_frame_header("\tat com.example.App.foo(App.java:10)") is True
    assert _is_frame_header("    at foo (/app/x.js:1:1)") is True


def test_stack_frame_header_rejects_prose():
    # `at ` 開頭但無括號 → 非 frame（擋掉 "at the store" 這類 prose）。
    assert _is_frame_header("at the store we bought milk") is False
    assert _is_frame_header("RecursionError: boom") is False
    assert _is_frame_header("Traceback (most recent call last):") is False


def test_stack_applies_on_recursion_trace():
    assert STACKTRACE.applies(_py_recursion_trace(15)) is True
    assert STACKTRACE.applies(_java_trace(15)) is True


def test_stack_applies_false_on_few_frames():
    # 少於 MIN_STACK_FRAMES → 不認領，交 truncate 兜底。
    assert STACKTRACE.applies(_py_recursion_trace(4)) is False


def test_stack_applies_false_on_prose():
    prose = "\n".join(f"at the park we saw {i} ducks today" for i in range(30))
    assert STACKTRACE.applies(prose) is False


def test_stack_squeeze_keeps_head_tail_frames_and_messages():
    from headroom_lite.strategies import STACK_KEEP_HEAD, STACK_KEEP_TAIL
    text = _py_recursion_trace(15)
    out = _stacktrace_squeeze(text)
    lines = out.split("\n")
    # 非 frame 訊號行全保留：標頭 + 最終錯誤行。
    assert lines[0] == "Traceback (most recent call last):"
    assert lines[-1] == "RecursionError: maximum recursion depth exceeded"
    # 頭 frame 保留（第一個 File 行）。
    assert '  File "/app/rec.py", line 3, in foo' in lines[1]
    # 中段 frame 收斂成單一 marker。
    assert any("dropped" in line and "stack frames" in line for line in lines)
    # 保留的 frame 數 = head + tail（每 frame 2 行）。
    file_lines = [line for line in lines if line.lstrip().startswith('File "')]
    assert len(file_lines) == STACK_KEEP_HEAD + STACK_KEEP_TAIL


def test_stack_marker_has_count_and_key():
    text = _py_recursion_trace(15)
    out = squeeze_text(text)
    assert f"sha256:{content_key(text)}" in out
    assert "dropped 9 stack frames" in out  # 15 - 3 - 3 = 9


def test_stack_deterministic():
    text = _py_recursion_trace(15)
    assert squeeze_text(text) == squeeze_text(text)


def test_stack_stores_original_before_squeezing():
    store = _SpyStore()
    text = _py_recursion_trace(15)
    squeeze_text(text, store=store)
    assert store.puts == [text]


def test_stack_no_drop_returns_text_without_put():
    store = _SpyStore()
    text = _py_recursion_trace(4)  # 太少 frame，不認領
    out = STACKTRACE.squeeze(text, store=store)
    assert out == text
    assert store.puts == []


def test_stack_does_not_swallow_logs():
    # 既有 noisy log（有 INFO/DEBUG 噪音）必須由 LOG 接走，不被 STACKTRACE 搶。
    log = "\n".join(
        [f"2026-06-27 INFO heartbeat {i}" for i in range(10)]
        + [f"2026-06-27 DEBUG poll {i}" for i in range(10)]
        + ["2026-06-27 ERROR boom"]
    )
    assert STACKTRACE.applies(log) is False


def test_stack_registered_after_log_before_truncate():
    names = [s.name for s in STRATEGIES]
    assert names.index("log") < names.index("stacktrace") < names.index("truncate")


def test_json_no_compress_returns_text_without_put():
    # 防禦性：array 太小壓不動 → 原文回、絕不 put。
    store = _SpyStore()
    text = _json_array(8)
    assert _json_squeeze(text, store) == text
    assert store.puts == []


def test_json_does_not_swallow_other_strategies():
    # 不回歸：log/diff/search 文字非 JSON 文件（不以 [/{ 開頭）→ JSON 不認領。
    assert JSON.applies(_noisy_log()) is False
    assert JSON.applies(_diff()) is False
    assert JSON.applies(_search()) is False


def test_json_registered_first():
    names = [s.name for s in STRATEGIES]
    assert names[0] == "json"
    assert names.index("json") < names.index("diff") < names.index("search") < names.index("log")


# ── M17：CSV/表格 內容感知策略（與 Rust tests/strategies.rs 對稱）──
from headroom_lite.strategies import (
    CSV,
    CSV_KEEP_HEAD,
    CSV_KEEP_TAIL,
    _csv_squeeze,
)


def _csv_table(rows: int = 40) -> str:
    """逗號分隔表格：1 表頭 + N 資料列，每列同欄數（4 欄 → 3 逗號）。"""
    header = "id,name,department,salary"
    body = "\n".join(f"{i},user{i},engineering,{50000 + i}" for i in range(rows))
    return f"{header}\n{body}"


def _tsv_table(rows: int = 40) -> str:
    """Tab 分隔表格：1 表頭 + N 資料列（3 欄 → 2 tab）。"""
    header = "id\tname\tcity"
    body = "\n".join(f"{i}\tuser{i}\ttaipei" for i in range(rows))
    return f"{header}\n{body}"


def test_csv_applies_on_comma_and_tab_tables():
    assert CSV.applies(_csv_table(40)) is True
    assert CSV.applies(_tsv_table(40)) is True


def test_csv_applies_false_on_few_droppable_rows():
    # 9 行（1 表頭 + 8 資料）：8 - 3 - 2 = 3 可丟 < MIN_CSV_DROP(4) → 不認領。
    assert CSV.applies(_csv_table(8)) is False


def test_csv_applies_false_on_prose():
    prose = "\n".join(f"the quick brown fox jumped {i}" for i in range(40))
    assert CSV.applies(prose) is False


def test_csv_applies_false_on_inconsistent_columns():
    # 每行逗號數不一致（散文夾雜逗號）→ 「每行同數」嗅探擋下，不認領。
    text = "\n".join(["a,b,c"] + [f"line {i}, one comma" for i in range(40)])
    assert CSV.applies(text) is False


def test_csv_applies_false_on_interior_blank_line():
    # 含內部空行 → 非乾淨表格，落 truncate 兜底。
    rows = [f"{i},user{i},eng,{i}" for i in range(40)]
    rows.insert(20, "")  # 中段插空行
    text = "id,name,dept,n\n" + "\n".join(rows)
    assert CSV.applies(text) is False


def test_csv_squeeze_keeps_header_head_tail():
    text = _csv_table(40)
    out = _csv_squeeze(text)
    lines = out.split("\n")
    assert lines[0] == "id,name,department,salary"  # 表頭恆保留
    assert lines[1] == "0,user0,engineering,50000"  # 第一筆資料列
    assert lines[-1] == "39,user39,engineering,50039"  # 最後一筆資料列
    assert any("dropped" in line and "table rows" in line for line in lines)
    # 輸出行數 = 表頭 + head + marker + tail。
    assert len(lines) == 1 + CSV_KEEP_HEAD + 1 + CSV_KEEP_TAIL


def test_csv_marker_has_count_and_key():
    text = _csv_table(40)
    out = squeeze_text(text)
    assert f"sha256:{content_key(text)}" in out
    assert "dropped 35 table rows" in out  # 40 - 3 - 2 = 35


def test_csv_deterministic():
    text = _csv_table(40)
    assert squeeze_text(text) == squeeze_text(text)


def test_csv_stores_original_before_squeezing():
    store = _SpyStore()
    text = _csv_table(40)
    squeeze_text(text, store=store)
    assert store.puts == [text]


def test_csv_no_compress_returns_text_without_put():
    # 可丟列數不足 → 原文回、絕不 put（神聖時機契約）。
    store = _SpyStore()
    text = _csv_table(8)
    assert _csv_squeeze(text, store) == text
    assert store.puts == []


def test_csv_does_not_swallow_other_strategies():
    # 不回歸：log/diff/search 由各自策略接走，csv 不搶（它們非「每行同逗號數」表格）。
    assert CSV.applies(_noisy_log()) is False
    assert CSV.applies(_diff()) is False
    assert CSV.applies(_search()) is False


def test_csv_registered_after_stacktrace_before_truncate():
    names = [s.name for s in STRATEGIES]
    assert names.index("stacktrace") < names.index("csv") < names.index("truncate")


# ── M18：Markdown table 內容感知策略（與 Rust tests/strategies.rs 對稱）──
from headroom_lite.strategies import (  # noqa: E402
    MARKDOWN,
    MD_KEEP_HEAD,
    MD_KEEP_TAIL,
    _md_squeeze,
)


def _md_table(rows: int = 40) -> str:
    """GitHub-flavored markdown 表格：表頭 + 分隔列 + N 資料列（皆 5 個 `|`）。"""
    header = "| id | name | department | salary |"
    sep = "| -- | ---- | ---------- | ------ |"
    body = "\n".join(f"| {i} | user{i} | engineering | {50000 + i} |" for i in range(rows))
    return f"{header}\n{sep}\n{body}"


def test_md_applies_on_markdown_table():
    assert MARKDOWN.applies(_md_table(40)) is True


def test_md_applies_false_on_few_droppable_rows():
    # 8 資料列：8 - 3 - 2 = 3 可丟 < MIN_MD_DROP(4) → 不認領。
    assert MARKDOWN.applies(_md_table(8)) is False


def test_md_applies_false_without_separator_row():
    # 第二行不是分隔列（沒有 dash）→ 不是 markdown 表格，落 truncate 兜底。
    rows = [f"| {i} | user{i} | eng |" for i in range(40)]
    text = "| id | name | dept |\n" + "\n".join(rows)  # 缺 |---| 分隔列
    assert MARKDOWN.applies(text) is False


def test_md_applies_false_on_inconsistent_pipes():
    # 每行 `|` 數不一致（散文夾雜 pipe）→「每行同數」嗅探擋下。
    text = "\n".join(["| a | b |", "| -- | -- |"] + [f"line {i} | one pipe" for i in range(40)])
    assert MARKDOWN.applies(text) is False


def test_md_applies_false_on_prose():
    prose = "\n".join(f"the quick brown fox jumped {i}" for i in range(40))
    assert MARKDOWN.applies(prose) is False


def test_md_squeeze_keeps_header_separator_head_tail():
    text = _md_table(40)
    out = _md_squeeze(text)
    lines = out.split("\n")
    assert lines[0] == "| id | name | department | salary |"  # 表頭恆保留
    assert lines[1] == "| -- | ---- | ---------- | ------ |"  # 分隔列恆保留（結構訊號）
    assert lines[2] == "| 0 | user0 | engineering | 50000 |"  # 第一筆資料列
    assert lines[-1] == "| 39 | user39 | engineering | 50039 |"  # 最後一筆資料列
    assert any("dropped" in line and "markdown table rows" in line for line in lines)
    # 輸出行數 = 表頭 + 分隔列 + head + marker + tail。
    assert len(lines) == 1 + 1 + MD_KEEP_HEAD + 1 + MD_KEEP_TAIL


def test_md_marker_has_count_and_key():
    text = _md_table(40)
    out = squeeze_text(text)
    assert f"sha256:{content_key(text)}" in out
    assert "dropped 35 markdown table rows" in out  # 40 - 3 - 2 = 35


def test_md_deterministic():
    text = _md_table(40)
    assert squeeze_text(text) == squeeze_text(text)


def test_md_stores_original_before_squeezing():
    store = _SpyStore()
    text = _md_table(40)
    squeeze_text(text, store=store)
    assert store.puts == [text]


def test_md_no_compress_returns_text_without_put():
    # 可丟列數不足 → 原文回、絕不 put（神聖時機契約）。
    store = _SpyStore()
    text = _md_table(8)
    assert _md_squeeze(text, store) == text
    assert store.puts == []


def test_md_does_not_swallow_other_strategies():
    # 不回歸：log/diff/search/csv 由各自策略接走，markdown 不搶（它們非 pipe 表格）。
    assert MARKDOWN.applies(_noisy_log()) is False
    assert MARKDOWN.applies(_diff()) is False
    assert MARKDOWN.applies(_search()) is False
    assert MARKDOWN.applies(_csv_table(40)) is False  # 逗號表格無 pipe → markdown 不認領


def test_md_registered_before_csv():
    # markdown 比 csv 更專一（需分隔列 + pipe 一致）→ 排 csv 前，兩者其實互斥（pipe vs 逗號）。
    names = [s.name for s in STRATEGIES]
    assert names.index("stacktrace") < names.index("markdown") < names.index("csv")
    assert names.index("csv") < names.index("truncate")


# ── M19：base64/hex blob 內容感知策略（與 Rust tests/strategies.rs 對稱）──
from headroom_lite.strategies import (  # noqa: E402
    BLOB,
    BLOB_HEAD,
    BLOB_TAIL,
    _blob_squeeze,
)

_B64_ALPHABET = (
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
)


def _blob(n: int = 2000) -> str:
    """data URI，內含 n 字元的確定性 base64 payload（單行、無換行/空白）。"""
    payload = "".join(_B64_ALPHABET[i % len(_B64_ALPHABET)] for i in range(n))
    return f"data:image/png;base64,{payload}"


def test_blob_applies_on_data_uri():
    assert BLOB.applies(_blob(2000)) is True


def test_blob_applies_false_on_short_run():
    # payload 100 < MIN_BLOB_RUN(512) → 不認領。
    assert BLOB.applies(_blob(100)) is False


def test_blob_applies_false_on_prose():
    # 散文含空白 → 連續 blob 串被打斷，無 512 字元長串。
    prose = " ".join(_B64_ALPHABET for _ in range(60))  # 每段 64 字元、被空白隔開
    assert BLOB.applies(prose) is False


def test_blob_applies_false_on_non_ascii():
    # 非 ASCII → 無法保證 char index == byte index → 不認領（落 truncate）。
    text = "中文" + ("A" * 2000) + "中文"
    assert BLOB.applies(text) is False


def test_blob_squeeze_keeps_head_tail():
    text = _blob(2000)
    out = _blob_squeeze(text)
    # 非 blob 前綴（data URI scheme）照抄保留。
    assert out.startswith("data:image/png;base64,")
    assert "blob chars" in out
    # 輸出遠短於原文（中段被丟）。
    assert len(out) < len(text)
    # 輸出結構精確 = 前綴 + blob 頭 + marker + blob 尾（中段 1872 字元被收斂掉）。
    prefix = "data:image/png;base64,"
    payload = text[len(prefix):]
    dropped = len(payload) - BLOB_HEAD - BLOB_TAIL
    marker = f"[... headroom-lite dropped {dropped} blob chars | sha256:{content_key(text)} ...]"
    assert out == prefix + payload[:BLOB_HEAD] + marker + payload[-BLOB_TAIL:]


def test_blob_marker_has_count_and_key():
    text = _blob(2000)
    out = squeeze_text(text)
    assert f"sha256:{content_key(text)}" in out
    assert "dropped 1872 blob chars" in out  # 2000 - 64 - 64 = 1872


def test_blob_deterministic():
    text = _blob(2000)
    assert squeeze_text(text) == squeeze_text(text)


def test_blob_stores_original_before_squeezing():
    store = _SpyStore()
    text = _blob(2000)
    squeeze_text(text, store=store)
    assert store.puts == [text]


def test_blob_no_compress_returns_text_without_put():
    # run 太短 → 原文回、絕不 put（神聖時機契約）。
    store = _SpyStore()
    text = _blob(100)
    assert _blob_squeeze(text, store) == text
    assert store.puts == []


def test_blob_does_not_swallow_other_strategies():
    # 不回歸：log/diff/search/csv/markdown 含空白/標點 → 無 512 連續 blob 串。
    assert BLOB.applies(_noisy_log()) is False
    assert BLOB.applies(_diff()) is False
    assert BLOB.applies(_search()) is False
    assert BLOB.applies(_csv_table(40)) is False
    assert BLOB.applies(_md_table(40)) is False


def test_blob_registered_after_csv_before_truncate():
    names = [s.name for s in STRATEGIES]
    assert names.index("csv") < names.index("blob") < names.index("truncate")


# ── M20：HTML/XML 內容感知策略（與 Rust tests/strategies.rs 對稱）──
from headroom_lite.strategies import (  # noqa: E402
    HTML,
    MIN_HTML_NOISE,
    _html_squeeze,
)


def _html_script(inner_len: int = 1000) -> str:
    """含單一巨型 inline <script> 的 HTML 文件；其餘為真實結構。"""
    inner = "a" * inner_len
    return (
        "<!DOCTYPE html>\n<html>\n<head>\n"
        f'<script type="text/javascript">{inner}</script>\n'
        "</head>\n<body>\n<h1>Title</h1>\n<p>Real content.</p>\n</body>\n</html>"
    )


def _html_style(inner_len: int = 1000) -> str:
    inner = ".x{color:red}" + "/* pad */" * ((inner_len // 8) + 1)
    return f"<html><head><style>{inner}</style></head><body><p>hi</p></body></html>"


def _html_comment(inner_len: int = 1000) -> str:
    inner = "x" * inner_len
    return f"<html><body><!--{inner}--><p>real</p></body></html>"


def test_html_applies_on_script_doc():
    assert HTML.applies(_html_script(1000)) is True


def test_html_applies_false_on_small_noise():
    # script 內文 < MIN_HTML_NOISE → 不認領。
    assert HTML.applies(_html_script(100)) is False


def test_html_applies_false_on_prose():
    prose = "\n".join(f"paragraph {i} of plain prose without markup" for i in range(40))
    assert HTML.applies(prose) is False


def test_html_squeeze_keeps_tags_drops_inner():
    text = _html_script(1000)
    out = _html_squeeze(text)
    assert '<script type="text/javascript">' in out  # 開標籤保留
    assert "</script>" in out  # 閉標籤保留
    assert "<h1>Title</h1>" in out  # 真實結構保留
    assert "aaaaaaaaaa" not in out  # 內文被丟
    assert "html noise chars" in out
    assert len(out) < len(text)


def test_html_marker_has_count_and_key():
    text = _html_script(1000)
    out = squeeze_text(text)
    assert f"sha256:{content_key(text)}" in out
    assert "dropped 1000 html noise chars" in out


def test_html_collapses_style():
    text = _html_style(1000)
    out = _html_squeeze(text)
    assert "<style>" in out and "</style>" in out
    assert "html noise chars" in out
    assert "<p>hi</p>" in out


def test_html_collapses_comment():
    text = _html_comment(1000)
    out = _html_squeeze(text)
    assert "<!--" in out and "-->" in out  # 註解邊界保留
    assert "xxxxxxxxxx" not in out  # 註解內文被丟
    assert "<p>real</p>" in out


def test_html_preserves_non_ascii():
    # 非 ASCII 文字內容（中文）須逐字保留 —— native-index 切片不依賴 ASCII。
    inner = "a" * 1000
    text = f"<html><body><h1>標題中文</h1><script>{inner}</script><p>內文</p></body></html>"
    out = _html_squeeze(text)
    assert "標題中文" in out
    assert "內文" in out
    assert "html noise chars" in out


def test_html_deterministic():
    text = _html_script(1000)
    assert squeeze_text(text) == squeeze_text(text)


def test_html_stores_original_before_squeezing():
    store = _SpyStore()
    text = _html_script(1000)
    squeeze_text(text, store=store)
    assert store.puts == [text]


def test_html_no_compress_returns_text_without_put():
    store = _SpyStore()
    text = _html_script(100)  # 內文太小
    assert _html_squeeze(text, store) == text
    assert store.puts == []


def test_html_does_not_swallow_other_strategies():
    # 不回歸：log/diff/search/csv/markdown/blob 皆無 <script>/<style>/<!-- 噪音區。
    assert HTML.applies(_noisy_log()) is False
    assert HTML.applies(_diff()) is False
    assert HTML.applies(_search()) is False
    assert HTML.applies(_csv_table(40)) is False
    assert HTML.applies(_md_table(40)) is False
    assert HTML.applies(_blob(2000)) is False


def test_html_registered_before_blob_after_csv():
    # HTML 排 blob 前：含 inline script 的頁面該走 HTML（保結構）、非被 blob 當巨串吞掉；
    # 但 data URI 無 <script> → HTML 不認領、落 blob。
    names = [s.name for s in STRATEGIES]
    assert names.index("csv") < names.index("html") < names.index("blob")
    assert names.index("blob") < names.index("truncate")
