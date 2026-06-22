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
    squeeze_text, squeeze_text_with, Strategy, DIFF, HEAD_LINES, LOG, STRATEGIES, TAIL_LINES,
    TRUNCATE,
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
