//! M11 — 壓縮策略 dispatcher（Rust port，對齊 Python `strategies.py`）。
//!
//! 從 M1/M5 的「寫死頭尾截斷」進化為「content sniffing → 選策略」的可插拔
//! 架構。每個策略是一對 function pointer：
//!   - applies(text)：這段內容適不適用本策略？（content sniffing）
//!   - squeeze(text)：確定性壓縮；回 `None` 代表「壓不動」（讓呼叫端保留原文）。
//!
//! 之後接 log / search / diff 等內容感知策略，只需多寫一個 `Strategy` 並
//! 插進 `STRATEGIES`（排在 truncate catch-all 之前），dispatcher 不必改。
//!
//! 與 Python 的差異（誠實記錄）：Rust 既有架構把 CCR 收存放在
//! `live_zone::compress_block`（squeeze 純函式、不碰 store），故此處策略的
//! squeeze 簽名只吃 `&str`；Python 則把收存放在 truncate 策略內。骨架階段
//! 兩邊各自沿用既有收存點，標記格式逐字相同 → parity 不受影響。
//!
//! 確定性契約：同輸入 → 同輸出（無時間戳、無隨機、content-hash keyed）。
//! 標記格式必須與 Python 版逐字相同 —— 跨語言 parity 的前提。

use crate::ccr::content_key;

pub const HEAD_LINES: usize = 20;
pub const TAIL_LINES: usize = 10;

/// 內容嗅探：這段文字適不適用本策略。
type AppliesFn = fn(&str) -> bool;
/// 確定性壓縮：回 `None` 代表壓不動（行數太少等），呼叫端保留原文。
type SqueezeFn = fn(&str) -> Option<String>;

/// 一個壓縮策略 = 內容嗅探 + 確定性壓縮。
///
/// 用具名 function pointer 對（而非 trait object）表達，刻意對稱 Python 側的
/// dataclass 註冊表，讓兩語言的 dispatcher 結構逐行對照。
pub struct Strategy {
    pub name: &'static str,
    pub applies: AppliesFn,
    pub squeeze: SqueezeFn,
}

/// catch-all：截斷對任何文字都適用，永遠回 true（殿後保底）。
fn truncate_applies(_text: &str) -> bool {
    true
}

/// 確定性頭尾截斷：頭 + 標記 + 尾。承襲 M5 live_zone 的 body 不變。
/// 行數不夠回 `None`（壓不動）。
fn truncate_squeeze(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= HEAD_LINES + TAIL_LINES {
        return None;
    }
    let omitted = lines.len() - HEAD_LINES - TAIL_LINES;
    let marker = format!(
        "[... headroom-lite squeezed {omitted} lines | sha256:{} ...]",
        content_key(text)
    );
    let mut parts: Vec<&str> = Vec::with_capacity(HEAD_LINES + TAIL_LINES + 1);
    parts.extend(&lines[..HEAD_LINES]);
    parts.push(&marker);
    parts.extend(&lines[lines.len() - TAIL_LINES..]);
    Some(parts.join("\n"))
}

/// truncate 是永遠適用的 catch-all，必須殿後（內容感知策略排它前面）。
pub const TRUNCATE: Strategy = Strategy {
    name: "truncate",
    applies: truncate_applies,
    squeeze: truncate_squeeze,
};

// ── M12 — log 內容感知策略（Rust port，對齊 Python `strategies.py`）──
//
// 與盲目頭尾截斷的差別：log 行可逐行依「嚴重度」分類。丟噪音（TRACE/DEBUG/INFO）、
// 留高嚴重度（WARN/ERROR/...）與其他行。散落「中段」的 ERROR —— truncate 只留頭尾
// 會一起丟掉，log 策略逐行嗅探把每個 error 都留下（內容感知取捨：寧多留 error）。
// 噪音佔比不夠高就不認領、讓 truncate 兜底。標記格式與 Python 版逐字相同 → parity。
//
// 收存點不對稱（誠實記錄，承襲 M11）：Rust squeeze 純函式（吃 &str、不碰 store），
// store.put 在 live_zone::compress_block 呼叫端（result.is_some() 時）；Python 把
// put 放策略內。兩邊在「同一條件下決定產出有損輸出」→ put 時機等價、parity 不破。

const MIN_LOG_LINES: usize = 6;
const LOG_RATIO: f64 = 0.6; // 可分類行佔非空行比例下限
const NOISE_RATIO: f64 = 0.3; // 可丟噪音行佔比下限 —— 低於此交給 truncate

const KEEP_TOKENS: [&[u8]; 5] = [b"WARNING", b"WARN", b"ERROR", b"FATAL", b"CRITICAL"];
const DROP_TOKENS: [&[u8]; 3] = [b"TRACE", b"DEBUG", b"INFO"];

/// token 是否以「整詞」出現在 line（前後皆字串邊界或非 ASCII 英數）。
/// 對 bytes 操作，與 Python `_contains_word` 逐字節對齊。
fn contains_word(line: &[u8], token: &[u8]) -> bool {
    let n = token.len();
    if n == 0 || line.len() < n {
        return false;
    }
    let mut start = 0;
    while start + n <= line.len() {
        let Some(off) = line[start..].windows(n).position(|w| w == token) else {
            return false;
        };
        let i = start + off;
        let before_ok = i == 0 || !line[i - 1].is_ascii_alphanumeric();
        let after_ok = i + n == line.len() || !line[i + n].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = i + 1;
    }
    false
}

/// 一行的嚴重度分類。
#[derive(PartialEq)]
enum Sev {
    Keep,
    Drop,
    Other,
}

fn severity(line: &str) -> Sev {
    let lb = line.as_bytes();
    if KEEP_TOKENS.iter().any(|t| contains_word(lb, t)) {
        return Sev::Keep;
    }
    if DROP_TOKENS.iter().any(|t| contains_word(lb, t)) {
        return Sev::Drop;
    }
    Sev::Other
}

/// 嗅探：夠多行像 log、且噪音佔比夠高（值得丟）才認領；否則讓 truncate 兜底。
fn log_applies(text: &str) -> bool {
    let nonempty: Vec<&str> = text.split('\n').filter(|l| !l.trim().is_empty()).collect();
    let total = nonempty.len();
    if total < MIN_LOG_LINES {
        return false;
    }
    let mut drop = 0usize;
    let mut classified = 0usize;
    for line in &nonempty {
        match severity(line) {
            Sev::Drop => {
                drop += 1;
                classified += 1;
            }
            Sev::Keep => classified += 1,
            Sev::Other => {}
        }
    }
    if drop == 0 {
        return false;
    }
    let total = total as f64;
    (classified as f64 / total) >= LOG_RATIO && (drop as f64 / total) >= NOISE_RATIO
}

/// 丟噪音行、保留高嚴重度與其他行，末尾附一行標記。
/// 沒噪音可丟回 `None`（呼叫端保留原文、不 put）。
fn log_squeeze(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let severities: Vec<Sev> = lines.iter().map(|l| severity(l)).collect();
    let dropped = severities.iter().filter(|s| **s == Sev::Drop).count();
    if dropped == 0 {
        return None;
    }
    let marker = format!(
        "[... headroom-lite dropped {dropped} log lines | sha256:{} ...]",
        content_key(text)
    );
    let mut parts: Vec<&str> = lines
        .iter()
        .zip(&severities)
        .filter(|(_, sev)| **sev != Sev::Drop)
        .map(|(line, _)| *line)
        .collect();
    parts.push(&marker);
    Some(parts.join("\n"))
}

/// log 是內容感知策略，排在 truncate catch-all 之前。
pub const LOG: Strategy = Strategy {
    name: "log",
    applies: log_applies,
    squeeze: log_squeeze,
};

/// 策略註冊表：按優先序排列。log 先嗅探，不命中才落到 truncate 兜底。
pub const STRATEGIES: &[Strategy] = &[LOG, TRUNCATE];

/// dispatcher：選第一個 applies 命中的策略來壓，命中即停。預設用模組級註冊表。
pub fn squeeze_text(text: &str) -> Option<String> {
    squeeze_text_with(text, STRATEGIES)
}

/// dispatcher 核心：可注入自訂策略順序（測試驗證 dispatch 行為）。
/// 無任何策略命中（理論上不會，truncate 是 catch-all）→ `None`。
pub fn squeeze_text_with(text: &str, strategies: &[Strategy]) -> Option<String> {
    for strategy in strategies {
        if (strategy.applies)(text) {
            return (strategy.squeeze)(text);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    //! 私有細節單元測試（整合測試碰不到私有 fn）：整詞比對的邊界正確性。
    use super::{severity, Sev};

    #[test]
    fn word_boundary_information_not_info() {
        // INFORMATION 不該被當成 INFO（整詞比對）；對齊 Python _severity 測試。
        assert!(matches!(severity("2026 INFORMATION about the system"), Sev::Other));
        assert!(matches!(severity("2026 INFO about the system"), Sev::Drop));
        assert!(matches!(severity("2026 WARNING disk almost full"), Sev::Keep));
    }
}
