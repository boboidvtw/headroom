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

/// 策略註冊表：按優先序排列。骨架階段只有 truncate；接內容感知策略時插在它前面。
pub const STRATEGIES: &[Strategy] = &[TRUNCATE];

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
