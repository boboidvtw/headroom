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
use std::collections::HashMap;

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

// ── M13 — diff 內容感知策略（Rust port，對齊 Python `strategies.py`）──
//
// 與盲目頭尾截斷的差別：unified/git diff 可逐行依「角色」分類。把未變更的
// context 行（` ` 空格開頭）丟掉、保留所有結構行：hunk header（`@@`）、檔頭
// （`diff`/`index`/`---`/`+++`）、與所有 `+`/`-` 變更行。散落在大段 context 中的
// 零星變更 —— truncate 只留頭尾會連同 context 一起丟；diff 策略逐行嗅探把每個變更
// 與 hunk header 都留下（hunk header 已編碼行號範圍可定位、CCR store 保有原文可逆）。
// context 佔比不夠高就不認領、讓 truncate 兜底。標記格式與 Python 版逐字相同 → parity。
//
// 與 log 對稱、刻意全用 ASCII byte 前綴比對（`starts_with(" ")` / `starts_with("@@")`），
// 不走 trim —— 避開 Python `strip()` 認 unicode 空白、與 Rust `trim` 分岔的地雷。
//
// 收存點不對稱（誠實記錄，承襲 M11/M12）：Rust squeeze 純函式（吃 &str、不碰 store），
// store.put 在 live_zone::compress_block 呼叫端（result.is_some() 時）；Python 把 put
// 放策略內。兩邊在「同一條件下決定產出有損輸出」→ put 時機等價、parity 不破。

const MIN_DIFF_LINES: usize = 6;
const DIFF_CONTEXT_RATIO: f64 = 0.3; // 可丟 context 行佔比下限 —— 低於此交給 truncate

/// 嗅探：像 unified diff（有 hunk header）、且 context 佔比夠高才認領；否則讓 truncate 兜底。
fn diff_applies(text: &str) -> bool {
    let lines: Vec<&str> = text.split('\n').collect();
    // hunk header（`@@ -a,b +c,d @@`）是 diff 的獨有信號 —— 沒有就不是 diff，避免誤判
    // markdown 的 `+`/`-` 條列。
    if !lines.iter().any(|l| l.starts_with("@@")) {
        return false;
    }
    let total = lines.len();
    if total < MIN_DIFF_LINES {
        return false;
    }
    let context = lines.iter().filter(|l| l.starts_with(' ')).count();
    if context == 0 {
        return false;
    }
    (context as f64 / total as f64) >= DIFF_CONTEXT_RATIO
}

/// 丟掉 context 行（` ` 空格開頭），保留 hunk header / 檔頭 / 所有 +/- 變更行，末尾附標記。
/// 沒 context 可丟回 `None`（呼叫端保留原文、不 put）。
fn diff_squeeze(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let dropped = lines.iter().filter(|l| l.starts_with(' ')).count();
    if dropped == 0 {
        return None;
    }
    let marker = format!(
        "[... headroom-lite dropped {dropped} diff context lines | sha256:{} ...]",
        content_key(text)
    );
    let mut parts: Vec<&str> = lines.iter().filter(|l| !l.starts_with(' ')).copied().collect();
    parts.push(&marker);
    Some(parts.join("\n"))
}

/// diff 是內容感知策略，排在 truncate catch-all 之前；亦排在 log 之前 —— 帶 `@@` 的
/// diff 結構比 log 嚴重度分類更該優先保留（既有 log 內容無 hunk header，互不干擾）。
pub const DIFF: Strategy = Strategy {
    name: "diff",
    applies: diff_applies,
    squeeze: diff_squeeze,
};

// ── M14 — search 內容感知策略（Rust port，對齊 Python `strategies.py`）──
//
// 對象：grep/rg 的 `file:lineno:content` 輸出。噪音 = 同一檔案的大量重複命中；訊號 =
// 命中分布在哪些檔、每檔代表性前幾筆。壓法：每檔保留前 KEEP_PER_FILE 筆、其餘丟，保序。
//
// parity 地雷（誠實記錄）：只用「`:` 分隔、第二欄全數字」會誤判 log 時間戳 `10:30:45`
// → search 反吃 log。解法：要求 file_key（首個 `:` 前）含 `/` —— 真 `grep -rn pat .` 必帶
// 路徑、時間戳前綴無 `/`。純 ASCII byte 檢查，兩語言一致。
//
// 確定性：保序逐行掃 + per-file 計數（HashMap 只做查找/累加、從不迭代）→ 無雜湊順序依賴。
// 收存點不對稱承襲 M11/12/13：Rust squeeze 純函式回 Option、put 在 compress_block 呼叫端。

const MIN_SEARCH_LINES: usize = 6;
const SEARCH_DROP_RATIO: f64 = 0.3;
const KEEP_PER_FILE: usize = 3;

/// grep/rg match 行判斷：回 `Some(file_key)` 或 `None`。
/// 形如 `file:lineno:content`，file_key 須含 `/`（排除時間戳）、lineno 須非空全 ASCII 數字。
fn match_line_key(line: &str) -> Option<&str> {
    let i1 = line.find(':')?;
    if i1 == 0 {
        return None; // file_key 為空（行首即冒號）
    }
    let file_key = &line[..i1];
    if !file_key.contains('/') {
        return None; // 必須像路徑 —— 擋掉 `10:30:45` 這類時間戳
    }
    let rest = &line[i1 + 1..];
    let i2 = rest.find(':')?;
    let lineno = &rest[..i2];
    if lineno.is_empty() || !lineno.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(file_key)
}

/// 保序逐行掃：每個 file_key 計數，超過 KEEP_PER_FILE 的 match 行標記為「丟」。
/// HashMap 只做查找/累加、從不迭代 —— 結果僅依輸入順序，無雜湊順序依賴（parity）。
fn search_drop_flags(lines: &[&str]) -> Vec<bool> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    lines
        .iter()
        .map(|line| match match_line_key(line) {
            Some(key) => {
                let c = counts.entry(key).or_insert(0);
                *c += 1;
                *c > KEEP_PER_FILE
            }
            None => false,
        })
        .collect()
}

/// 嗅探：夠多行、且超出每檔上限的可丟命中佔比夠高才認領；否則讓後手兜底。
fn search_applies(text: &str) -> bool {
    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len();
    if total < MIN_SEARCH_LINES {
        return false;
    }
    let dropped = search_drop_flags(&lines).iter().filter(|d| **d).count();
    if dropped == 0 {
        return false;
    }
    (dropped as f64 / total as f64) >= SEARCH_DROP_RATIO
}

/// 每檔保留前 KEEP_PER_FILE 筆命中、丟其餘，末尾附標記。
/// 沒超量可丟回 `None`（呼叫端保留原文、不 put）。
fn search_squeeze(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let flags = search_drop_flags(&lines);
    let dropped = flags.iter().filter(|d| **d).count();
    if dropped == 0 {
        return None;
    }
    let marker = format!(
        "[... headroom-lite dropped {dropped} search result lines | sha256:{} ...]",
        content_key(text)
    );
    let mut parts: Vec<&str> = lines
        .iter()
        .zip(&flags)
        .filter(|(_, drop)| !**drop)
        .map(|(line, _)| *line)
        .collect();
    parts.push(&marker);
    Some(parts.join("\n"))
}

/// search 排在 diff 之後、log 之前：grep-over-logs 由 search 接管；既有 log 內容無
/// `/`+數字 match 行 → search 不認領，不回歸 M12 行為。
pub const SEARCH: Strategy = Strategy {
    name: "search",
    applies: search_applies,
    squeeze: search_squeeze,
};

// ── M15 — json 內容感知策略（Rust port，對齊 Python `strategies.py`）──
//
// 對象：大型 JSON 文件。找元素最多的 array，保前 JSON_HEAD + 後 JSON_TAIL 個元素、中間塞
// marker 字串元素（結果仍合法 JSON），array 外 bytes 照抄。
//
// ⭐ parity 正解：**絕不重序列化任何值**。地雷是「json.dumps 把 1.10 正規化成 1.1，Rust
// arbitrary_precision 保留 1.10 → 分岔」。解法不是硬扛 number encoder，而是 byte-level 結構
// 掃描——被保留元素照抄原始 bytes 切片，唯一新寫的是結構字元與 marker（ASCII 常數）。
//
// tie-break：元素最多；同票取 start 最小（嚴格大於才替換 → 保源序最前；Python max 與 Rust
// max_by_key 同票行為相反，故兩邊都顯式用此規則）。掃描器追蹤字串字面值與巢狀深度。
// 收存點不對稱承襲 M11–M14：核心純函式回 Option、put 在 compress_block 呼叫端。

const JSON_HEAD: usize = 5;
const JSON_TAIL: usize = 2;
const MIN_JSON_DROP: usize = 4;

/// 一個 array 的掃描結果：(start, end, 各元素的 (start, end) byte span)。
type ArraySpan = (usize, usize, Vec<(usize, usize)>);

/// 首個非 ASCII 空白 byte 是否為 `[`/`{`（判斷整段 content 是不是 JSON 文件）。
fn starts_json(text: &str) -> bool {
    for b in text.bytes() {
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            b'[' | b'{' => return true,
            _ => return false,
        }
    }
    false
}

/// array 掃描 frame：物件只需 kind 佔深度，array 才追蹤元素 span。
struct ArrFrame {
    kind: u8,
    start: usize,
    elements: Vec<(usize, usize)>,
    elem_start: usize,
}

/// 單次線性掃描，回所有 JSON array 的 (start, end, elem_spans)（byte offset）。
/// 只在「目前最內層是 array」時把逗號當元素分隔；跳過字串字面值內字元。
fn scan_arrays(text: &str) -> Vec<ArraySpan> {
    let bytes = text.as_bytes();
    let mut arrays: Vec<ArraySpan> = Vec::new();
    let mut stack: Vec<ArrFrame> = Vec::new();
    let (mut in_string, mut escape) = (false, false);
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'[' | b'{' => stack.push(ArrFrame {
                kind: c,
                start: i,
                elements: Vec::new(),
                elem_start: i + 1,
            }),
            b']' | b'}' => {
                if let Some(mut frame) = stack.pop() {
                    if frame.kind == b'[' && c == b']' {
                        let s = frame.elem_start;
                        // 空 array（[]）或只有空白 → 不計為元素
                        if !frame.elements.is_empty() || !text[s..i].trim().is_empty() {
                            frame.elements.push((s, i));
                        }
                        arrays.push((frame.start, i + 1, frame.elements));
                    }
                }
            }
            b',' => {
                if let Some(frame) = stack.last_mut() {
                    if frame.kind == b'[' {
                        frame.elements.push((frame.elem_start, i));
                        frame.elem_start = i + 1;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    arrays
}

/// 純函式：找元素最多的 array 截斷成頭+marker+尾。壓不動回 `None`。
fn json_squeeze_core(text: &str) -> Option<String> {
    if !starts_json(text) {
        return None;
    }
    let arrays = scan_arrays(text);
    // tie-break：元素最多；同票保源序最前（嚴格大於才替換）。
    let mut best: Option<&ArraySpan> = None;
    for arr in &arrays {
        if best.is_none() || arr.2.len() > best.unwrap().2.len() {
            best = Some(arr);
        }
    }
    let best = best?;
    let (start, end, elems) = (best.0, best.1, &best.2);
    if elems.len() < JSON_HEAD + JSON_TAIL + MIN_JSON_DROP {
        return None;
    }
    let dropped = elems.len() - JSON_HEAD - JSON_TAIL;
    let marker = format!(
        "\"[... headroom-lite dropped {dropped} array elements | sha256:{} ...]\"",
        content_key(text)
    );
    let mut parts: Vec<&str> = Vec::with_capacity(JSON_HEAD + JSON_TAIL + 1);
    for &(s, e) in &elems[..JSON_HEAD] {
        parts.push(&text[s..e]);
    }
    parts.push(&marker);
    for &(s, e) in &elems[elems.len() - JSON_TAIL..] {
        parts.push(&text[s..e]);
    }
    let new_array = format!("[{}]", parts.join(","));
    Some(format!("{}{}{}", &text[..start], new_array, &text[end..]))
}

/// 嗅探：是 JSON 文件、且最大 array 元素夠多（可丟 ≥ MIN_JSON_DROP）才認領。
fn json_applies(text: &str) -> bool {
    json_squeeze_core(text).is_some()
}

/// 截斷最大 array；壓不動回 `None`（呼叫端保留原文、不 put）。
fn json_squeeze(text: &str) -> Option<String> {
    json_squeeze_core(text)
}

/// json 排最前：applies 極專一（需首字元 `[`/`{` + 11+ 元素 array），不會誤搶 diff/search/log。
pub const JSON: Strategy = Strategy {
    name: "json",
    applies: json_applies,
    squeeze: json_squeeze,
};

/// 策略註冊表：按優先序排列。json/diff/search/log 先嗅探，不命中才落到 truncate 兜底。
pub const STRATEGIES: &[Strategy] = &[JSON, DIFF, SEARCH, LOG, TRUNCATE];

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
