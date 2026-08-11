//! M11 安全網（加嚴版）：壓縮策略 dispatcher（Rust port，對齊 Python test_strategies.py）。
//!
//! 鎖住的不變式：
//!   1. dispatcher 把活派給「第一個 applies 命中」的策略，命中即停。
//!   2. truncate 是永遠適用的 catch-all（殿後保底）。
//!   3. squeeze 回 None 代表壓不動（行數太少）。
//!   4. 確定性 + 標記含 content_key（含 unicode 輸入）。
//!   5. 門檻邊界：剛好 HEAD+TAIL 行不壓、多一行才壓。

use headroom_lite_rs::ccr::content_key;
use headroom_lite_rs::strategies::{
    squeeze_text, squeeze_text_with, Strategy, BLOB, CSV, DIFF, HEAD_LINES, HTML, JSON, LOG,
    MARKDOWN, SEARCH, STACKTRACE, STRATEGIES, TAIL_LINES, TRUNCATE,
};

fn long_text() -> String {
    (0..100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
}
fn short_text() -> String {
    (0..5).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
}

#[test]
fn truncate_applies_is_catch_all() {
    assert!((TRUNCATE.applies)(&long_text()));
    assert!((TRUNCATE.applies)(&short_text()));
    assert!((TRUNCATE.applies)(""));
}

#[test]
fn dispatcher_routes_to_first_matching_strategy() {
    fn dummy_applies(_: &str) -> bool {
        true
    }
    fn dummy_squeeze(_: &str) -> Option<String> {
        Some("DUMMY-WON".to_string())
    }
    let dummy = Strategy {
        name: "dummy",
        applies: dummy_applies,
        squeeze: dummy_squeeze,
    };
    let strategies = [dummy, TRUNCATE];
    assert_eq!(
        squeeze_text_with(&long_text(), &strategies).as_deref(),
        Some("DUMMY-WON")
    );
}

#[test]
fn dispatcher_skips_non_matching_strategy() {
    fn never_applies(_: &str) -> bool {
        false
    }
    fn never_squeeze(_: &str) -> Option<String> {
        Some("SHOULD-NOT-RUN".to_string())
    }
    let never = Strategy {
        name: "never",
        applies: never_applies,
        squeeze: never_squeeze,
    };
    let strategies = [never, TRUNCATE];
    let out = squeeze_text_with(&long_text(), &strategies).expect("truncate should fire");
    assert!(out.contains("headroom-lite squeezed"));
}

#[test]
fn no_strategy_matches_returns_none() {
    fn never_applies(_: &str) -> bool {
        false
    }
    fn never_squeeze(_: &str) -> Option<String> {
        Some("X".to_string())
    }
    let never = Strategy {
        name: "never",
        applies: never_applies,
        squeeze: never_squeeze,
    };
    assert_eq!(squeeze_text_with(&long_text(), &[never]), None);
}

#[test]
fn truncate_marker_contains_content_key() {
    let text = long_text();
    let out = squeeze_text(&text).expect("long text compresses");
    assert!(out.contains(&format!("sha256:{}", content_key(&text))));
    assert!(out.contains("headroom-lite squeezed"));
}

#[test]
fn below_threshold_returns_none() {
    assert_eq!(squeeze_text(&short_text()), None);
}

#[test]
fn deterministic_same_input_same_output() {
    let text = long_text();
    assert_eq!(squeeze_text(&text), squeeze_text(&text));
}

// ── 加嚴：門檻邊界（剛好 HEAD+TAIL 行 vs 多一行）──
#[test]
fn exactly_at_threshold_not_compressed() {
    let n = HEAD_LINES + TAIL_LINES;
    let text = (0..n).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
    assert_eq!(squeeze_text(&text), None, "剛好 {n} 行不該壓");
}

#[test]
fn one_over_threshold_is_compressed() {
    let n = HEAD_LINES + TAIL_LINES + 1;
    let text = (0..n).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
    let out = squeeze_text(&text).expect("超門檻一行該壓");
    assert!(out.contains("squeezed 1 lines"), "省略行數應為 1，得 {out}");
}

// ── 加嚴：unicode 輸入的確定性與 key 一致 ──
#[test]
fn unicode_text_deterministic_and_keyed() {
    let line = "中文行 \u{1f338} emoji";
    let text = (0..50).map(|i| format!("{line} {i}")).collect::<Vec<_>>().join("\n");
    let a = squeeze_text(&text).expect("unicode 長文該壓");
    let b = squeeze_text(&text).expect("unicode 長文該壓");
    assert_eq!(a, b, "unicode 輸入必須確定性");
    assert!(a.contains(&format!("sha256:{}", content_key(&text))));
}

// ── M12：log 內容感知策略（對齊 Python test_strategies.py）──

/// 噪音夾雜的 log：error 刻意埋在「中段」（truncate 的盲區）。
fn noisy_log_n(n_errors: usize, n_noise: usize) -> String {
    let half = n_noise / 2;
    let mut lines: Vec<String> = (0..half)
        .map(|i| format!("2026-06-20 10:00:{i:02} DEBUG worker tick {i}"))
        .collect();
    lines.extend((0..n_errors).map(|i| format!("2026-06-20 10:01:{i:02} ERROR db connection failed attempt {i}")));
    lines.extend((0..n_noise - half).map(|i| format!("2026-06-20 10:02:{i:02} INFO retrying job {i}")));
    lines.join("\n")
}
fn noisy_log() -> String {
    noisy_log_n(5, 20)
}

#[test]
fn log_applies_on_noisy_log() {
    assert!((LOG.applies)(&noisy_log()));
}

#[test]
fn log_applies_false_on_prose() {
    let prose = (0..30)
        .map(|i| format!("This is sentence number {i} about nothing in particular."))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!(LOG.applies)(&prose));
}

#[test]
fn log_applies_false_when_no_noise() {
    // 全 ERROR：沒噪音可丟 → 不認領，交給 truncate 兜底。
    let all_err = (0..30).map(|i| format!("ERROR something broke {i}")).collect::<Vec<_>>().join("\n");
    assert!(!(LOG.applies)(&all_err));
}

#[test]
fn log_squeeze_drops_noise_keeps_errors() {
    let out = (LOG.squeeze)(&noisy_log()).expect("noisy log 該壓");
    assert!(!out.contains("DEBUG"));
    assert!(!out.contains("INFO"));
    assert_eq!(out.matches("ERROR").count(), 5);
}

#[test]
fn log_keeps_middle_errors_unlike_truncate() {
    // 3 個 error 埋在 60 行噪音中段；truncate 頭尾保留會丟掉，log 全留。
    let log = noisy_log_n(3, 60);
    let out = squeeze_text(&log).expect("noisy log 該壓");
    assert_eq!(out.matches("ERROR").count(), 3);
    assert!(out.contains("dropped")); // 走 log 策略，不是 truncate 的 "squeezed"
}

#[test]
fn log_marker_has_count_and_key() {
    let log = noisy_log();
    let out = squeeze_text(&log).expect("noisy log 該壓");
    assert!(out.contains(&format!("sha256:{}", content_key(&log))));
    assert!(out.contains("dropped 20 log lines"));
}

#[test]
fn log_no_drop_returns_none() {
    // 防禦性：squeeze 直呼但無噪音可丟 → None（呼叫端保留原文、不 put）。
    let all_err = (0..10).map(|i| format!("ERROR x {i}")).collect::<Vec<_>>().join("\n");
    assert_eq!((LOG.squeeze)(&all_err), None);
}

#[test]
fn log_registered_before_truncate() {
    let names: Vec<&str> = STRATEGIES.iter().map(|s| s.name).collect();
    let log_i = names.iter().position(|&n| n == "log").expect("log 已註冊");
    let trunc_i = names.iter().position(|&n| n == "truncate").expect("truncate 已註冊");
    assert!(log_i < trunc_i, "log 必須排在 truncate 之前");
}

#[test]
fn log_deterministic() {
    let log = noisy_log();
    assert_eq!(squeeze_text(&log), squeeze_text(&log));
}

// ── M13：diff 內容感知策略（對齊 Python test_strategies.py）──

/// context 夾雜的 unified diff：變更刻意埋在「中段」（truncate 的盲區）。
fn diff_n(n_context: usize, n_changes: usize) -> String {
    let half = n_context / 2;
    let mut lines: Vec<String> = vec![
        "diff --git a/app.py b/app.py".into(),
        "index 1111111..2222222 100644".into(),
        "--- a/app.py".into(),
        "+++ b/app.py".into(),
        "@@ -1,40 +1,40 @@ def main():".into(),
    ];
    lines.extend((0..half).map(|i| format!(" context line {i} unchanged")));
    lines.extend((0..n_changes).map(|i| format!("-old line {i}")));
    lines.extend((0..n_changes).map(|i| format!("+new line {i}")));
    lines.extend((half..n_context).map(|i| format!(" context line {i} unchanged")));
    lines.join("\n")
}
fn diff() -> String {
    diff_n(20, 4)
}

#[test]
fn diff_applies_on_unified_diff() {
    assert!((DIFF.applies)(&diff()));
}

#[test]
fn diff_applies_false_without_hunk_header() {
    // 像 diff 的 +/- 但沒 hunk header（如 markdown 條列）→ 不認領。
    let no_hunk = (0..30)
        .map(|i| if i % 2 == 1 { format!("- bullet {i}") } else { format!("+ bullet {i}") })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!(DIFF.applies)(&no_hunk));
}

#[test]
fn diff_applies_false_when_no_context() {
    // 全變更行、無 context 可丟 → 不認領，交給後手兜底。
    let all_changes =
        format!("@@ -1,5 +1,5 @@\n{}", (0..30).map(|i| format!("+line {i}")).collect::<Vec<_>>().join("\n"));
    assert!(!(DIFF.applies)(&all_changes));
}

#[test]
fn diff_squeeze_drops_context_keeps_changes() {
    let out = (DIFF.squeeze)(&diff()).expect("diff 該壓");
    assert!(!out.contains("unchanged")); // 未變更的 context 全丟（標記文字含 "context" 故查內容字串）
    assert_eq!(out.matches("-old line").count(), 4);
    assert_eq!(out.matches("+new line").count(), 4);
    assert!(out.contains("@@ -1,40 +1,40 @@"));
    assert!(out.contains("diff --git a/app.py b/app.py"));
}

#[test]
fn diff_keeps_middle_changes_unlike_truncate() {
    // 變更埋在大段 context 中段；truncate 頭尾保留會丟掉，diff 全留。
    let d = diff_n(60, 3);
    let out = squeeze_text(&d).expect("diff 該壓");
    assert_eq!(out.matches("-old line").count(), 3);
    assert_eq!(out.matches("+new line").count(), 3);
    assert!(out.contains("diff context lines")); // 走 diff 策略，不是 truncate 的 "squeezed"
}

#[test]
fn diff_marker_has_count_and_key() {
    let d = diff();
    let out = squeeze_text(&d).expect("diff 該壓");
    assert!(out.contains(&format!("sha256:{}", content_key(&d))));
    assert!(out.contains("dropped 20 diff context lines"));
}

#[test]
fn diff_no_drop_returns_none() {
    // 防禦性：squeeze 直呼但無 context 可丟 → None（呼叫端保留原文、不 put）。
    let all_changes =
        format!("@@ -1,5 +1,5 @@\n{}", (0..10).map(|i| format!("+line {i}")).collect::<Vec<_>>().join("\n"));
    assert_eq!((DIFF.squeeze)(&all_changes), None);
}

#[test]
fn diff_registered_before_log_and_truncate() {
    let names: Vec<&str> = STRATEGIES.iter().map(|s| s.name).collect();
    let diff_i = names.iter().position(|&n| n == "diff").expect("diff 已註冊");
    let log_i = names.iter().position(|&n| n == "log").expect("log 已註冊");
    let trunc_i = names.iter().position(|&n| n == "truncate").expect("truncate 已註冊");
    assert!(diff_i < log_i && log_i < trunc_i, "順序須為 diff < log < truncate");
}

#[test]
fn diff_deterministic() {
    let d = diff();
    assert_eq!(squeeze_text(&d), squeeze_text(&d));
}

// ── M14：search 內容感知策略（對齊 Python test_strategies.py）──

/// grep/rg 風格輸出：每檔多筆命中（超過 KEEP_PER_FILE → 可丟）。
fn search_text(n_files: usize, per_file: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for f in 0..n_files {
        for ln in 0..per_file {
            lines.push(format!("./src/module_{f}.py:{}:    result = compute(value_{ln})", ln + 1));
        }
    }
    lines.join("\n")
}
fn search_default() -> String {
    search_text(3, 12)
}

#[test]
fn search_applies_on_grep_output() {
    assert!((SEARCH.applies)(&search_default()));
}

// ── M24：錨定行號標記，認得 rg -C 的 `-` 分隔 context 行（對齊 Python）──
//
// 病：`rg -C` 的 context 行用 `-` 分隔，不被舊判準認得 → 未認出的行灌大比率閘門的
// 分母 → 策略在最該生效時自己關掉、落盲目截斷。實測 10 檔 8 命中：ctx=0 佔比 0.625
// 認領，ctx=1 掉到 0.208、ctx=4 掉到 0.069，全部落截斷。

/// `rg -C <ctx>` 風格輸出：命中行用 `:` 分隔、context 行用 `-` 分隔。
fn rg_context(n_files: usize, per_file: usize, ctx: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for f in 0..n_files {
        for m in 0..per_file {
            let ln = (m + 1) * 10;
            for c in (1..=ctx).rev() {
                lines.push(format!("./src/module_{f}.py-{}-    before_{c}", ln - c));
            }
            lines.push(format!("./src/module_{f}.py:{ln}:    result = compute(value_{m})"));
            for c in 1..=ctx {
                lines.push(format!("./src/module_{f}.py-{}-    after_{c}", ln + c));
            }
        }
    }
    lines.join("\n")
}

#[test]
fn search_applies_on_rg_context_output() {
    // 病灶正面測：rg -C 1..4 全部都要認領（M24 之前全是 false）。
    for ctx in 1..=4 {
        assert!(
            (SEARCH.applies)(&rg_context(3, 12, ctx)),
            "rg -C {ctx} 未被認領"
        );
    }
}

#[test]
fn search_context_lines_do_not_crowd_out_real_matches() {
    // context 是命中的附屬，不該當獨立命中去佔 KEEP_PER_FILE 額度
    // （否則每檔留的 3 行是 context/match/context，真命中只剩 1 筆）。
    let out = (SEARCH.squeeze)(&rg_context(3, 12, 1)).expect("rg -C 輸出該壓");
    for f in 0..3 {
        assert_eq!(
            out.matches(&format!("./src/module_{f}.py:")).count(),
            3,
            "每檔該保留 KEEP_PER_FILE 個真命中"
        );
    }
}

#[test]
fn search_kept_matches_keep_their_context() {
    // 保留的命中連同 context 一起保；被丟的命中其 context 一起丟（不留孤兒）。
    let out = (SEARCH.squeeze)(&rg_context(1, 12, 1)).expect("rg -C 輸出該壓");
    for m in 0..3 {
        let ln = (m + 1) * 10;
        assert!(out.contains(&format!("./src/module_0.py-{}-    before_1", ln - 1)));
        assert!(out.contains(&format!("./src/module_0.py-{}-    after_1", ln + 1)));
    }
    for m in 3..12 {
        let ln = (m + 1) * 10;
        assert!(!out.contains(&format!("./src/module_0.py-{}-", ln - 1)));
        assert!(!out.contains(&format!("./src/module_0.py-{}-", ln + 1)));
    }
}

#[test]
fn search_plain_grep_not_regressed() {
    // 另一側：純 grep（無 context 行）必須維持認領，M14 行為不得回歸。
    assert!((SEARCH.applies)(&search_default()));
}

#[test]
fn search_claims_grep_output_under_a_path_with_spaces() {
    // M24 回歸守門：M14 認領含空白的路徑（macOS 常見），M24 不得讓它退步。
    // 空白防線只套 `-` 型標記（時間戳的威脅面），不套 `:` 型。
    let text: String = (0..3)
        .flat_map(|f| {
            (0..12).map(move |ln| {
                format!("./my dir/module_{f}.py:{}:    result = compute(value_{ln})", ln + 1)
            })
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        (SEARCH.applies)(&text),
        "含空白路徑的 grep 輸出仍須被 search 認領"
    );
}

#[test]
fn search_rejects_log_line_containing_a_path() {
    // M24 自帶的新誤判面：錨定後 prefix 是「標記前的一切」，於是含路徑的 log 行會被
    // `-06-` 錨中且含 `/`。防線＝path 段不得含空白。用「整段是否被 search 認領」驗證
    // （match_line_key 是私有函式，測公開行為）。
    let log: String = (0..40)
        .map(|i| format!("/usr/src/app.py 2026-06-20 event {i} happened"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!(SEARCH.applies)(&log), "含路徑的 log 行不得被 search 認領");
}

#[test]
fn search_applies_false_on_prose() {
    let prose = (0..30)
        .map(|i| format!("This is sentence number {i} about nothing."))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!(SEARCH.applies)(&prose));
}

#[test]
fn search_applies_false_when_under_cap() {
    // 每檔只 2 筆（≤ KEEP_PER_FILE）→ 無可丟，不認領。
    assert!(!(SEARCH.applies)(&search_text(5, 2)));
}

#[test]
fn search_squeeze_caps_per_file() {
    let out = (SEARCH.squeeze)(&search_text(3, 12)).expect("grep 輸出該壓");
    // 每檔保留恰好 3 筆（KEEP_PER_FILE）。
    for f in 0..3 {
        assert_eq!(out.matches(&format!("./src/module_{f}.py:")).count(), 3);
    }
    assert!(out.contains("dropped"));
}

#[test]
fn search_marker_has_count_and_key() {
    let text = search_text(3, 12); // 每檔丟 9 → 共丟 27
    let out = squeeze_text(&text).expect("grep 輸出該壓");
    assert!(out.contains(&format!("sha256:{}", content_key(&text))));
    assert!(out.contains("dropped 27 search result lines"));
}

#[test]
fn search_no_drop_returns_none() {
    // 防禦性：squeeze 直呼但無超量可丟 → None（呼叫端保留原文、不 put）。
    assert_eq!((SEARCH.squeeze)(&search_text(4, 2)), None);
}

#[test]
fn search_does_not_swallow_logs() {
    // 關鍵不回歸：噪音 log 仍走 log（含時間戳但無 /+數字 match 行）→ search 不吃。
    let log = noisy_log();
    assert!(!(SEARCH.applies)(&log));
    let out = squeeze_text(&log).expect("log 該壓");
    assert!(out.contains("log lines")); // 仍是 log 策略的標記
}

#[test]
fn search_registered_after_diff_before_log() {
    let names: Vec<&str> = STRATEGIES.iter().map(|s| s.name).collect();
    let d = names.iter().position(|&n| n == "diff").unwrap();
    let s = names.iter().position(|&n| n == "search").unwrap();
    let l = names.iter().position(|&n| n == "log").unwrap();
    let t = names.iter().position(|&n| n == "truncate").unwrap();
    assert!(d < s && s < l && l < t, "順序須為 diff < search < log < truncate");
}

#[test]
fn search_deterministic() {
    let text = search_default();
    assert_eq!(squeeze_text(&text), squeeze_text(&text));
}

// ── M15：json 內容感知策略（對齊 Python test_strategies.py）──

/// 同質物件的大型 compact JSON array（模擬 API 回應）。
fn json_array(n: usize) -> String {
    let items: Vec<String> = (0..n)
        .map(|i| format!("{{\"id\":{i},\"name\":\"item_{i}\",\"active\":{}}}", i % 2 == 0))
        .collect();
    format!("[{}]", items.join(","))
}

#[test]
fn json_applies_on_large_array() {
    assert!((JSON.applies)(&json_array(20)));
}

#[test]
fn json_applies_false_on_small_array() {
    // 元素不足 11（HEAD5+TAIL2+DROP4）→ 不認領。
    assert!(!(JSON.applies)(&json_array(8)));
}

#[test]
fn json_applies_false_on_prose() {
    let prose = (0..30)
        .map(|i| format!("sentence {i} with [brackets] and, commas"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!(JSON.applies)(&prose));
}

#[test]
fn json_squeeze_keeps_head_tail() {
    let out = (JSON.squeeze)(&json_array(20)).expect("大 array 該壓");
    // 頭元素原文保留、尾元素保留、中間 marker。
    assert!(out.starts_with("[{\"id\":0,\"name\":\"item_0\",\"active\":true}"));
    assert!(out.ends_with("{\"id\":19,\"name\":\"item_19\",\"active\":false}]"));
    assert!(out.contains("dropped 13 array elements"));
}

#[test]
fn json_never_reserializes_numbers() {
    // ⭐ parity 正解驗證：1.10 照抄原文、不被正規化成 1.1。
    let elems: Vec<&str> = (0..20).map(|_| "{\"v\":1.10}").collect();
    let text = format!("[{}]", elems.join(","));
    let out = json_squeeze_core_via_squeeze(&text);
    assert!(out.contains("1.10"));
}

/// 透過公開的 JSON.squeeze 取核心輸出（core 為私有）。
fn json_squeeze_core_via_squeeze(text: &str) -> String {
    (JSON.squeeze)(text).expect("該壓")
}

#[test]
fn json_nested_picks_largest_array() {
    // 物件內含大 array → 找到並截斷它、外層結構照抄。
    let text = format!("{{\"meta\":{{\"n\":3}},\"data\":{},\"ok\":true}}", json_array(20));
    let out = (JSON.squeeze)(&text).expect("巢狀大 array 該壓");
    assert!(out.starts_with("{\"meta\":{\"n\":3},\"data\":["));
    assert!(out.ends_with(",\"ok\":true}"));
    assert!(out.contains("dropped 13 array elements"));
}

#[test]
fn json_marker_has_count_and_key() {
    let text = json_array(20);
    let out = squeeze_text(&text).expect("大 array 該壓");
    assert!(out.contains(&format!("sha256:{}", content_key(&text))));
    assert!(out.contains("dropped 13 array elements"));
}

#[test]
fn json_no_compress_returns_none() {
    // 防禦性：array 太小壓不動 → None（呼叫端保留原文、不 put）。
    assert_eq!((JSON.squeeze)(&json_array(8)), None);
}

#[test]
fn json_does_not_swallow_other_strategies() {
    // 不回歸：log/diff/search 文字非 JSON 文件（不以 [/{ 開頭）→ JSON 不認領。
    assert!(!(JSON.applies)(&noisy_log()));
    assert!(!(JSON.applies)(&diff()));
    assert!(!(JSON.applies)(&search_default()));
}

#[test]
fn json_registered_first() {
    let names: Vec<&str> = STRATEGIES.iter().map(|s| s.name).collect();
    assert_eq!(names[0], "json");
    let j = names.iter().position(|&n| n == "json").unwrap();
    let d = names.iter().position(|&n| n == "diff").unwrap();
    assert!(j < d, "json 須排最前");
}

#[test]
fn json_deterministic() {
    let text = json_array(20);
    assert_eq!(squeeze_text(&text), squeeze_text(&text));
}

// ── M16：stack trace 內容感知策略（對齊 Python test_strategies.py）──

/// Python RecursionError traceback：N 個逐字相同的 2 行 frame（典型遞迴爆炸）。
fn py_recursion_trace(frames: usize) -> String {
    let head = "Traceback (most recent call last):";
    let body: Vec<&str> = (0..frames)
        .map(|_| "  File \"/app/rec.py\", line 3, in foo\n    return foo(n - 1)")
        .collect();
    let tail = "RecursionError: maximum recursion depth exceeded";
    format!("{head}\n{}\n{tail}", body.join("\n"))
}

#[test]
fn stack_applies_on_recursion_trace() {
    assert!((STACKTRACE.applies)(&py_recursion_trace(15)));
}

#[test]
fn stack_applies_false_on_few_frames() {
    // 少於 MIN_STACK_FRAMES → 不認領，交 truncate 兜底。
    assert!(!(STACKTRACE.applies)(&py_recursion_trace(4)));
}

#[test]
fn stack_applies_false_on_prose() {
    let prose = (0..30)
        .map(|i| format!("at the park we saw {i} ducks today"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!(STACKTRACE.applies)(&prose));
}

#[test]
fn stack_squeeze_keeps_head_tail_and_messages() {
    let text = py_recursion_trace(15);
    let out = (STACKTRACE.squeeze)(&text).expect("recursion trace 該壓");
    let lines: Vec<&str> = out.split('\n').collect();
    // 非 frame 訊號行全保留：標頭 + 最終錯誤行。
    assert_eq!(lines[0], "Traceback (most recent call last):");
    assert_eq!(
        *lines.last().unwrap(),
        "RecursionError: maximum recursion depth exceeded"
    );
    // 保留的 File 行數 = head + tail（3+3）。
    let file_lines = lines
        .iter()
        .filter(|l| l.trim_start().starts_with("File \""))
        .count();
    assert_eq!(file_lines, 6);
    assert!(out.contains("stack frames"));
}

#[test]
fn stack_marker_has_count_and_key() {
    let text = py_recursion_trace(15);
    let out = squeeze_text(&text).expect("recursion trace 該壓");
    assert!(out.contains(&format!("sha256:{}", content_key(&text))));
    assert!(out.contains("dropped 9 stack frames")); // 15 - 3 - 3 = 9
}

#[test]
fn stack_no_drop_returns_none() {
    // 防禦性：squeeze 直呼但 frame 太少 → None（呼叫端保留原文、不 put）。
    assert_eq!((STACKTRACE.squeeze)(&py_recursion_trace(4)), None);
}

#[test]
fn stack_does_not_swallow_logs() {
    // 關鍵不回歸：噪音 log 仍走 log → stacktrace 不吃。
    let log = noisy_log();
    assert!(!(STACKTRACE.applies)(&log));
    let out = squeeze_text(&log).expect("log 該壓");
    assert!(out.contains("log lines")); // 仍是 log 策略的標記
}

#[test]
fn stack_registered_after_log_before_truncate() {
    let names: Vec<&str> = STRATEGIES.iter().map(|s| s.name).collect();
    let l = names.iter().position(|&n| n == "log").unwrap();
    let s = names.iter().position(|&n| n == "stacktrace").unwrap();
    let t = names.iter().position(|&n| n == "truncate").unwrap();
    assert!(l < s && s < t, "順序須為 log < stacktrace < truncate");
}

#[test]
fn stack_deterministic() {
    let text = py_recursion_trace(15);
    assert_eq!(squeeze_text(&text), squeeze_text(&text));
}

// ── M17：CSV/表格 內容感知策略（對齊 Python test_strategies.py）──

/// 逗號分隔表格：1 表頭 + N 資料列，每列同欄數（4 欄 → 3 逗號）。
fn csv_table(rows: usize) -> String {
    let header = "id,name,department,salary";
    let body: Vec<String> = (0..rows)
        .map(|i| format!("{i},user{i},engineering,{}", 50000 + i))
        .collect();
    format!("{header}\n{}", body.join("\n"))
}

/// Tab 分隔表格：1 表頭 + N 資料列（3 欄 → 2 tab）。
fn tsv_table(rows: usize) -> String {
    let header = "id\tname\tcity";
    let body: Vec<String> = (0..rows).map(|i| format!("{i}\tuser{i}\ttaipei")).collect();
    format!("{header}\n{}", body.join("\n"))
}

#[test]
fn csv_applies_on_comma_and_tab_tables() {
    assert!((CSV.applies)(&csv_table(40)));
    assert!((CSV.applies)(&tsv_table(40)));
}

#[test]
fn csv_applies_false_on_few_droppable_rows() {
    // 8 資料列：8 - 3 - 2 = 3 可丟 < MIN_CSV_DROP(4) → 不認領。
    assert!(!(CSV.applies)(&csv_table(8)));
}

#[test]
fn csv_applies_false_on_prose() {
    let prose = (0..40)
        .map(|i| format!("the quick brown fox jumped {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!(CSV.applies)(&prose));
}

#[test]
fn csv_applies_false_on_inconsistent_columns() {
    // 每行逗號數不一致 → 「每行同數」嗅探擋下。
    let mut lines = vec!["a,b,c".to_string()];
    lines.extend((0..40).map(|i| format!("line {i}, one comma")));
    assert!(!(CSV.applies)(&lines.join("\n")));
}

#[test]
fn csv_applies_false_on_interior_blank_line() {
    // 含內部空行 → 非乾淨表格，落 truncate 兜底。
    let mut rows: Vec<String> = (0..40).map(|i| format!("{i},user{i},eng,{i}")).collect();
    rows.insert(20, String::new());
    let text = format!("id,name,dept,n\n{}", rows.join("\n"));
    assert!(!(CSV.applies)(&text));
}

#[test]
fn csv_squeeze_keeps_header_head_tail() {
    let text = csv_table(40);
    let out = (CSV.squeeze)(&text).expect("表格該壓");
    let lines: Vec<&str> = out.split('\n').collect();
    assert_eq!(lines[0], "id,name,department,salary"); // 表頭恆保留
    assert_eq!(lines[1], "0,user0,engineering,50000"); // 第一筆資料列
    assert_eq!(*lines.last().unwrap(), "39,user39,engineering,50039"); // 最後一筆
    assert!(out.contains("table rows"));
    // 輸出行數 = 表頭 + head(3) + marker + tail(2) = 7。
    assert_eq!(lines.len(), 1 + 3 + 1 + 2);
}

#[test]
fn csv_marker_has_count_and_key() {
    let text = csv_table(40);
    let out = squeeze_text(&text).expect("表格該壓");
    assert!(out.contains(&format!("sha256:{}", content_key(&text))));
    assert!(out.contains("dropped 35 table rows")); // 40 - 3 - 2 = 35
}

#[test]
fn csv_no_drop_returns_none() {
    // 防禦性：squeeze 直呼但可丟列數不足 → None（呼叫端保留原文、不 put）。
    assert_eq!((CSV.squeeze)(&csv_table(8)), None);
}

#[test]
fn csv_does_not_swallow_other_strategies() {
    // 不回歸：log/diff/search 非「每行同逗號數」表格 → csv 不搶。
    assert!(!(CSV.applies)(&noisy_log()));
    assert!(!(CSV.applies)(&diff()));
    assert!(!(CSV.applies)(&search_default()));
}

#[test]
fn csv_registered_after_stacktrace_before_truncate() {
    let names: Vec<&str> = STRATEGIES.iter().map(|s| s.name).collect();
    let s = names.iter().position(|&n| n == "stacktrace").unwrap();
    let c = names.iter().position(|&n| n == "csv").unwrap();
    let t = names.iter().position(|&n| n == "truncate").unwrap();
    assert!(s < c && c < t, "順序須為 stacktrace < csv < truncate");
}

#[test]
fn csv_deterministic() {
    let text = csv_table(40);
    assert_eq!(squeeze_text(&text), squeeze_text(&text));
}

// ── M18：Markdown table 內容感知策略（對齊 Python test_strategies.py）──

/// GitHub-flavored markdown 表格：表頭 + 分隔列 + N 資料列（皆 5 個 `|`）。
fn md_table(rows: usize) -> String {
    let header = "| id | name | department | salary |";
    let sep = "| -- | ---- | ---------- | ------ |";
    let body: Vec<String> = (0..rows)
        .map(|i| format!("| {i} | user{i} | engineering | {} |", 50000 + i))
        .collect();
    format!("{header}\n{sep}\n{}", body.join("\n"))
}

#[test]
fn md_applies_on_markdown_table() {
    assert!((MARKDOWN.applies)(&md_table(40)));
}

#[test]
fn md_applies_false_on_few_droppable_rows() {
    // 8 資料列：8 - 3 - 2 = 3 可丟 < MIN_MD_DROP(4) → 不認領。
    assert!(!(MARKDOWN.applies)(&md_table(8)));
}

#[test]
fn md_applies_false_without_separator_row() {
    // 第二行不是分隔列（沒有 dash）→ 不是 markdown 表格。
    let mut lines = vec!["| id | name | dept |".to_string()];
    lines.extend((0..40).map(|i| format!("| {i} | user{i} | eng |")));
    assert!(!(MARKDOWN.applies)(&lines.join("\n")));
}

#[test]
fn md_applies_false_on_inconsistent_pipes() {
    // 每行 `|` 數不一致 → 「每行同數」嗅探擋下。
    let mut lines = vec!["| a | b |".to_string(), "| -- | -- |".to_string()];
    lines.extend((0..40).map(|i| format!("line {i} | one pipe")));
    assert!(!(MARKDOWN.applies)(&lines.join("\n")));
}

#[test]
fn md_applies_false_on_prose() {
    let prose = (0..40)
        .map(|i| format!("the quick brown fox jumped {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!(MARKDOWN.applies)(&prose));
}

#[test]
fn md_squeeze_keeps_header_separator_head_tail() {
    let text = md_table(40);
    let out = (MARKDOWN.squeeze)(&text).expect("markdown 表格該壓");
    let lines: Vec<&str> = out.split('\n').collect();
    assert_eq!(lines[0], "| id | name | department | salary |"); // 表頭恆保留
    assert_eq!(lines[1], "| -- | ---- | ---------- | ------ |"); // 分隔列恆保留
    assert_eq!(lines[2], "| 0 | user0 | engineering | 50000 |"); // 第一筆資料列
    assert_eq!(*lines.last().unwrap(), "| 39 | user39 | engineering | 50039 |"); // 最後一筆
    assert!(out.contains("markdown table rows"));
    // 輸出行數 = 表頭 + 分隔列 + head(3) + marker + tail(2) = 8。
    assert_eq!(lines.len(), 1 + 1 + 3 + 1 + 2);
}

#[test]
fn md_marker_has_count_and_key() {
    let text = md_table(40);
    let out = squeeze_text(&text).expect("markdown 表格該壓");
    assert!(out.contains(&format!("sha256:{}", content_key(&text))));
    assert!(out.contains("dropped 35 markdown table rows")); // 40 - 3 - 2 = 35
}

#[test]
fn md_no_drop_returns_none() {
    // 防禦性：squeeze 直呼但可丟列數不足 → None（呼叫端保留原文、不 put）。
    assert_eq!((MARKDOWN.squeeze)(&md_table(8)), None);
}

#[test]
fn md_does_not_swallow_other_strategies() {
    // 不回歸：log/diff/search/csv 非 pipe 表格 → markdown 不搶。
    assert!(!(MARKDOWN.applies)(&noisy_log()));
    assert!(!(MARKDOWN.applies)(&diff()));
    assert!(!(MARKDOWN.applies)(&search_default()));
    assert!(!(MARKDOWN.applies)(&csv_table(40))); // 逗號表格無 pipe → markdown 不認領
}

#[test]
fn md_registered_before_csv() {
    let names: Vec<&str> = STRATEGIES.iter().map(|s| s.name).collect();
    let s = names.iter().position(|&n| n == "stacktrace").unwrap();
    let m = names.iter().position(|&n| n == "markdown").unwrap();
    let c = names.iter().position(|&n| n == "csv").unwrap();
    let t = names.iter().position(|&n| n == "truncate").unwrap();
    assert!(s < m && m < c && c < t, "順序須為 stacktrace < markdown < csv < truncate");
}

#[test]
fn md_deterministic() {
    let text = md_table(40);
    assert_eq!(squeeze_text(&text), squeeze_text(&text));
}

// ── M19：base64/hex blob 內容感知策略（對齊 Python test_strategies.py）──

const B64_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// data URI，內含 n 字元的確定性 base64 payload（單行、無換行/空白）。
fn blob_uri(n: usize) -> String {
    let payload: String = (0..n).map(|i| B64_ALPHABET[i % B64_ALPHABET.len()] as char).collect();
    format!("data:image/png;base64,{payload}")
}

#[test]
fn blob_applies_on_data_uri() {
    assert!((BLOB.applies)(&blob_uri(2000)));
}

#[test]
fn blob_applies_false_on_short_run() {
    // payload 100 < MIN_BLOB_RUN(512) → 不認領。
    assert!(!(BLOB.applies)(&blob_uri(100)));
}

#[test]
fn blob_applies_false_on_prose() {
    // 散文含空白 → 連續 blob 串被打斷，無 512 字元長串。
    let seg = std::str::from_utf8(B64_ALPHABET).unwrap();
    let prose = vec![seg; 60].join(" ");
    assert!(!(BLOB.applies)(&prose));
}

#[test]
fn blob_applies_false_on_non_ascii() {
    // 非 ASCII → 無法保證 char index == byte index → 不認領。
    let text = format!("中文{}中文", "A".repeat(2000));
    assert!(!(BLOB.applies)(&text));
}

#[test]
fn blob_squeeze_keeps_head_tail() {
    let text = blob_uri(2000);
    let out = (BLOB.squeeze)(&text).expect("blob 該壓");
    assert!(out.starts_with("data:image/png;base64,"));
    assert!(out.contains("blob chars"));
    assert!(out.len() < text.len());
    // 輸出結構精確 = 前綴 + blob 頭 + marker + blob 尾。
    let prefix = "data:image/png;base64,";
    let payload = &text[prefix.len()..];
    let dropped = payload.len() - 64 - 64;
    let marker = format!(
        "[... headroom-lite dropped {dropped} blob chars | sha256:{} ...]",
        content_key(&text)
    );
    let expected = format!("{prefix}{}{marker}{}", &payload[..64], &payload[payload.len() - 64..]);
    assert_eq!(out, expected);
}

#[test]
fn blob_marker_has_count_and_key() {
    let text = blob_uri(2000);
    let out = squeeze_text(&text).expect("blob 該壓");
    assert!(out.contains(&format!("sha256:{}", content_key(&text))));
    assert!(out.contains("dropped 1872 blob chars")); // 2000 - 64 - 64 = 1872
}

#[test]
fn blob_no_compress_returns_none() {
    // 防禦性：squeeze 直呼但 run 太短 → None（呼叫端保留原文、不 put）。
    assert_eq!((BLOB.squeeze)(&blob_uri(100)), None);
}

#[test]
fn blob_does_not_swallow_other_strategies() {
    // 不回歸：log/diff/search/csv/markdown 含空白/標點 → 無 512 連續 blob 串。
    assert!(!(BLOB.applies)(&noisy_log()));
    assert!(!(BLOB.applies)(&diff()));
    assert!(!(BLOB.applies)(&search_default()));
    assert!(!(BLOB.applies)(&csv_table(40)));
    assert!(!(BLOB.applies)(&md_table(40)));
}

#[test]
fn blob_registered_after_csv_before_truncate() {
    let names: Vec<&str> = STRATEGIES.iter().map(|s| s.name).collect();
    let c = names.iter().position(|&n| n == "csv").unwrap();
    let b = names.iter().position(|&n| n == "blob").unwrap();
    let t = names.iter().position(|&n| n == "truncate").unwrap();
    assert!(c < b && b < t, "順序須為 csv < blob < truncate");
}

#[test]
fn blob_deterministic() {
    let text = blob_uri(2000);
    assert_eq!(squeeze_text(&text), squeeze_text(&text));
}

// ── M20：HTML/XML 內容感知策略（對齊 Python test_strategies.py）──

/// 含單一巨型 inline <script> 的 HTML 文件；其餘為真實結構。
fn html_script(inner_len: usize) -> String {
    let inner = "a".repeat(inner_len);
    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n\
         <script type=\"text/javascript\">{inner}</script>\n\
         </head>\n<body>\n<h1>Title</h1>\n<p>Real content.</p>\n</body>\n</html>"
    )
}

fn html_style(inner_len: usize) -> String {
    let inner = format!(".x{{color:red}}{}", "/* pad */".repeat(inner_len / 8 + 1));
    format!("<html><head><style>{inner}</style></head><body><p>hi</p></body></html>")
}

fn html_comment(inner_len: usize) -> String {
    let inner = "x".repeat(inner_len);
    format!("<html><body><!--{inner}--><p>real</p></body></html>")
}

#[test]
fn html_applies_on_script_doc() {
    assert!((HTML.applies)(&html_script(1000)));
}

#[test]
fn html_applies_false_on_small_noise() {
    assert!(!(HTML.applies)(&html_script(100)));
}

#[test]
fn html_applies_false_on_prose() {
    let prose = (0..40)
        .map(|i| format!("paragraph {i} of plain prose without markup"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!(HTML.applies)(&prose));
}

#[test]
fn html_squeeze_keeps_tags_drops_inner() {
    let text = html_script(1000);
    let out = (HTML.squeeze)(&text).expect("HTML 該壓");
    assert!(out.contains("<script type=\"text/javascript\">"));
    assert!(out.contains("</script>"));
    assert!(out.contains("<h1>Title</h1>"));
    assert!(!out.contains("aaaaaaaaaa"));
    assert!(out.contains("html noise chars"));
    assert!(out.len() < text.len());
}

#[test]
fn html_marker_has_count_and_key() {
    let text = html_script(1000);
    let out = squeeze_text(&text).expect("HTML 該壓");
    assert!(out.contains(&format!("sha256:{}", content_key(&text))));
    assert!(out.contains("dropped 1000 html noise chars"));
}

#[test]
fn html_collapses_style() {
    let text = html_style(1000);
    let out = (HTML.squeeze)(&text).expect("style 該壓");
    assert!(out.contains("<style>") && out.contains("</style>"));
    assert!(out.contains("html noise chars"));
    assert!(out.contains("<p>hi</p>"));
}

#[test]
fn html_collapses_comment() {
    let text = html_comment(1000);
    let out = (HTML.squeeze)(&text).expect("comment 該壓");
    assert!(out.contains("<!--") && out.contains("-->"));
    assert!(!out.contains("xxxxxxxxxx"));
    assert!(out.contains("<p>real</p>"));
}

#[test]
fn html_preserves_non_ascii() {
    // 非 ASCII 文字內容（中文）須逐字保留 —— byte-index 切片在 ASCII 標籤邊界、不破 UTF-8。
    let inner = "a".repeat(1000);
    let text =
        format!("<html><body><h1>標題中文</h1><script>{inner}</script><p>內文</p></body></html>");
    let out = (HTML.squeeze)(&text).expect("HTML 該壓");
    assert!(out.contains("標題中文"));
    assert!(out.contains("內文"));
    assert!(out.contains("html noise chars"));
}

#[test]
fn html_deterministic() {
    let text = html_script(1000);
    assert_eq!(squeeze_text(&text), squeeze_text(&text));
}

#[test]
fn html_no_compress_returns_none() {
    assert_eq!((HTML.squeeze)(&html_script(100)), None);
}

#[test]
fn html_does_not_swallow_other_strategies() {
    assert!(!(HTML.applies)(&noisy_log()));
    assert!(!(HTML.applies)(&diff()));
    assert!(!(HTML.applies)(&search_default()));
    assert!(!(HTML.applies)(&csv_table(40)));
    assert!(!(HTML.applies)(&md_table(40)));
    assert!(!(HTML.applies)(&blob_uri(2000)));
}

#[test]
fn html_registered_before_blob_after_csv() {
    let names: Vec<&str> = STRATEGIES.iter().map(|s| s.name).collect();
    let c = names.iter().position(|&n| n == "csv").unwrap();
    let h = names.iter().position(|&n| n == "html").unwrap();
    let b = names.iter().position(|&n| n == "blob").unwrap();
    let t = names.iter().position(|&n| n == "truncate").unwrap();
    assert!(c < h && h < b && b < t, "順序須為 csv < html < blob < truncate");
}

// ── M21 — 建置/測試輸出的進度行 ──
//
// 補上一整類過去沒有的斷言：「它該認領的輸入，它有認領」。既有 log 測試全都先餵
// 保證含 ERROR/DEBUG token 的輸入再驗行為，於是「對 pytest 輸出整個不啟動」
// 這種失效可以長期綠燈潛伏。缺陷實錄見 rewrite/READING-02-log-compressor.md。

fn pytest_output() -> String {
    let mut v: Vec<String> =
        vec!["============================= test session starts ==============================".into()];
    for i in 1..20 {
        v.push(format!("tests/test_mod{i}.py {}    [{:3}%]", ".".repeat(40), i * 2));
    }
    v.push("=================================== FAILURES ===================================".into());
    v.push(">       assert a == b".into());
    v.push("E       AssertionError: assert 'a1b2' == 'c3d4'".into());
    v.push("tests/test_cache.py:42: AssertionError".into());
    for i in 1..20 {
        v.push(format!("tests/test_late{i}.py {}   [100%]", ".".repeat(40)));
    }
    v.push("========================= 1 failed, 153 passed in 0.31s ========================".into());
    v.join("\n")
}

#[test]
fn log_applies_on_pytest_output() {
    // 缺陷本體：修補前這裡是 false，輸入落到盲目頭尾截斷。
    assert!((LOG.applies)(&pytest_output()));
}

#[test]
fn pytest_failures_survive_squeeze() {
    // 真正的契約：FAILURES 在中段，盲目頭尾截斷會整段吃掉它。
    let out = squeeze_text(&pytest_output()).expect("pytest 輸出應被壓縮");
    assert!(out.contains("FAILURES"), "FAILURES 區塊必須存活");
    assert!(out.contains("AssertionError"));
    assert!(out.contains("tests/test_cache.py:42"));
    assert!(out.contains("dropped"), "須走 log 策略而非 truncate");
}

#[test]
fn long_dot_leader_is_not_a_progress_line() {
    // 參數空間另一側：目錄點狀填充可以很長，光看連續長度會誤判。
    let toc = std::iter::repeat_n("Chapter 3 .......................... 42", 20)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!(LOG.applies)(&toc));
}

#[test]
fn progress_line_strip_is_ascii_only_like_python() {
    // parity 地雷：Python bytes.strip() 只剝 ASCII 空白，str::trim() 會剝 U+3000。
    // 全形空格結尾的裸進度行 → 兩語言都必須判為「非進度行」。
    let line = format!("{}\u{3000}", ".".repeat(20));
    let text = std::iter::repeat_n(line.as_str(), 20).collect::<Vec<_>>().join("\n");
    assert!(!(LOG.applies)(&text), "U+3000 收尾不得被剝掉而誤判為進度行");
}

// ── M22 — 罕見即資訊（對齊 Python test_strategies.py）──
//
// 動機見 READING-03：頭 5 尾 2 的依據是「排在第幾個」，3 筆 timeout 埋在中段就全滅，
// 而壓縮率 92% 讓它看起來像成功。判準取自 smart_crusher 修好 Bug #3 之後的 Pareto 檢查。

fn records(n: usize, rare_at: &[usize]) -> String {
    let mut rows: Vec<String> = Vec::new();
    for i in 0..n {
        if rare_at.contains(&i) {
            rows.push(format!(
                r#"{{"id": {i}, "endpoint": "/api/v1/res{i}", "status": "timeout", "error": "upstream did not respond"}}"#
            ));
        } else {
            rows.push(format!(
                r#"{{"id": {i}, "endpoint": "/api/v1/res{i}", "status": "ok"}}"#
            ));
        }
    }
    format!(r#"{{"results": [{}]}}"#, rows.join(", "))
}

#[test]
fn json_keeps_rare_status_elements() {
    let out = squeeze_text(&records(100, &[48, 49, 50])).expect("該壓");
    assert_eq!(out.matches(r#""status": "timeout""#).count(), 3, "三筆罕見值全部必須保留");
    assert!(out.contains("upstream did not respond"));
}

#[test]
fn json_rare_skips_uniform_field() {
    // 該過的還要過：每筆 name 都不同 → 非類別欄，不得觸發（09_json fixture 的形狀）。
    let rows: Vec<String> = (0..24)
        .map(|i| format!(r#"{{"id": {i}, "name": "row_{i}"}}"#))
        .collect();
    let text = format!(r#"{{"data": [{}]}}"#, rows.join(", "));
    let out = squeeze_text(&text).expect("該壓");
    assert_eq!(out.matches("headroom-lite dropped").count(), 1, "單一連續丟棄段 → 一個 marker");
}

#[test]
fn json_rare_skips_high_cardinality_id_field() {
    let rows: Vec<String> = (0..60)
        .map(|i| format!(r#"{{"i": {i}, "uuid": "id-{i:04}"}}"#))
        .collect();
    let text = format!(r#"{{"data": [{}]}}"#, rows.join(", "));
    let out = squeeze_text(&text).expect("該壓");
    assert_eq!(out.matches("headroom-lite dropped").count(), 1);
}

#[test]
fn json_rare_bimodal_case() {
    // Bug #3 舊版整個漏掉的情況；罕見值刻意擺中段，擺尾端會被 tail 順手撈到而假通過。
    let mut rows: Vec<String> = (0..60).map(|i| format!(r#"{{"i": {i}, "lvl": "info"}}"#)).collect();
    rows.extend((0..15).map(|i| format!(r#"{{"i": {}, "lvl": "err_{i}"}}"#, 200 + i)));
    rows.extend((0..25).map(|i| format!(r#"{{"i": {}, "lvl": "warn"}}"#, 100 + i)));
    let text = format!(r#"{{"data": [{}]}}"#, rows.join(", "));
    let out = squeeze_text(&text).expect("該壓");
    assert_eq!(out.matches("err_").count(), 15, "雙峰分布下 15 個罕見錯誤全部必須保留");
}

#[test]
fn json_rare_marks_each_dropped_run() {
    let out = squeeze_text(&records(100, &[48, 49, 50])).expect("該壓");
    assert_eq!(out.matches("headroom-lite dropped").count(), 2, "中段有必留元素 → 兩段丟棄");
}

#[test]
fn json_rare_deterministic() {
    let text = records(100, &[48, 49, 50]);
    assert_eq!(squeeze_text(&text), squeeze_text(&text));
}
