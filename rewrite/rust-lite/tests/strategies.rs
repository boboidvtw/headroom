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
    squeeze_text, squeeze_text_with, Strategy, HEAD_LINES, LOG, STRATEGIES, TAIL_LINES, TRUNCATE,
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
