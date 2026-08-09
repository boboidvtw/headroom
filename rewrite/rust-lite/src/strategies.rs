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
use std::collections::{BTreeMap, BTreeSet, HashMap};

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

// ── M21 — 建置/測試輸出的進度行 ──
//
// severity token 表是為「應用程式 runtime log」設計的體裁；pytest 說 FAILED、
// cargo 說 `error[E0382]`、jest 用符號 —— 一個都不命中，於是 log 策略對建置/測試
// 輸出整個不認領、落到盲目頭尾截斷，把中段的 FAILURES 丟掉。缺陷實錄見 READING-02。
//
// 刻意不擴充 token 表（開放集合，補完 pytest 還有下一個），改用結構訊號。
// 兩個條件同時成立才算，因為光看連續長度會誤判目錄的點狀填充：
//   1. 存在長度 >= MIN_PROGRESS_RUN 的進度符號連續段，且
//   2. 該行以 `%]` 收尾，或整行只由進度符號與空白組成。
//
// 與 Python `_is_progress_line` 逐字節對齊（純 ASCII byte 視角）。

const MIN_PROGRESS_RUN: usize = 8;
const PROGRESS_GLYPHS: &[u8] = b".sxXFEP";

fn is_progress_line(line: &str) -> bool {
    let lb = line.as_bytes();
    let (mut run, mut best) = (0usize, 0usize);
    for b in lb {
        if PROGRESS_GLYPHS.contains(b) {
            run += 1;
            best = best.max(run);
        } else {
            run = 0;
        }
    }
    if best < MIN_PROGRESS_RUN {
        return false;
    }
    // 刻意不用 str::trim()：它剝的是 Unicode 空白（含 U+3000 全形空格），而 Python 的
    // bytes.strip() 只剝 ASCII 的 b" \t\n\r\x0b\x0c"。差一個字元就會讓同一行輸入
    // 在兩語言分岔 —— parity 是逐字節的，這裡必須自己剝。
    // 注意 \x0b 不在 Rust 的 is_ascii_whitespace() 裡，得明列。
    const ASCII_STRIP: &[u8] = b" \t\n\r\x0b\x0c";
    let mut s = lb;
    while let Some((f, rest)) = s.split_first() {
        if ASCII_STRIP.contains(f) {
            s = rest;
        } else {
            break;
        }
    }
    while let Some((l, rest)) = s.split_last() {
        if ASCII_STRIP.contains(l) {
            s = rest;
        } else {
            break;
        }
    }
    if s.ends_with(b"%]") {
        return true;
    }
    s.iter()
        .all(|b| PROGRESS_GLYPHS.contains(b) || *b == b' ' || *b == b'\t')
}

/// 順序：keep token 優先（嚴重度勝過形狀）→ 進度行 → drop token → other。
fn severity(line: &str) -> Sev {
    let lb = line.as_bytes();
    if KEEP_TOKENS.iter().any(|t| contains_word(lb, t)) {
        return Sev::Keep;
    }
    if is_progress_line(line) {
        return Sev::Drop;
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

// ── M22 — 罕見即資訊（第一個「按資訊選擇」的判準）──
//
// 頭 5 尾 2 的依據是「排在第幾個」。100 筆健檢裡 3 筆 timeout 埋在中段 → 壓縮率 92%
// 而三筆全滅、留下七筆一樣的 ok，模型會得出「一切正常」。輸出看起來完全合理、指標
// 還很漂亮，這正是它比 M21 那個缺陷更危險的地方（見 READING-03）。
//
// 判準取自 smart_crusher 的 detect_rare_status_values，且用的是它**修好 Bug #3 之後**
// 的版本：原版 `if not (2 <= len(unique_values) <= 10): continue` 會讓「保留罕見錯誤」
// 在錯誤種類一多時自己關掉 —— 而那正是最需要它的時候。改用 Pareto 檢查。
//
// parity：80% 門檻用整數運算避免浮點分岔；BTreeMap/顯式排序，絕不依賴雜湊順序。

const RARE_MAX_CARDINALITY: usize = 50;
const RARE_COVERAGE_PCT: usize = 80;
const RARE_MAX_K: usize = 5;
// 上限與判準同源：Pareto 已保證單一欄位的罕見值 <= 20%，但多個類別欄的聯集可能超過，
// 所以對聯集再套一次 20%。刻意不用絕對值上限 —— 那會在罕見值剛好 15 個時安靜丟掉 5 個
// 最有資訊量的元素，正是這個策略要避免的事。
const RARE_MAX_KEEP_PCT: usize = 20;

/// 從一個元素的原文抽出所有 `"key": "value"` 字串對（value 非字串者略過）。
/// 刻意不解析 JSON —— 全程只做括號/字串掃描，與 Python `_json_string_pairs` 逐字節對齊。
fn json_string_pairs(elem: &str) -> Vec<(&str, &str)> {
    let b = elem.as_bytes();
    let n = b.len();
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    let mut pending: Option<&str> = None;
    let mut i = 0usize;
    while i < n {
        if b[i] == b'"' {
            let mut j = i + 1;
            let mut esc = false;
            while j < n {
                if esc {
                    esc = false;
                } else if b[j] == b'\\' {
                    esc = true;
                } else if b[j] == b'"' {
                    break;
                }
                j += 1;
            }
            let lit = &elem[i + 1..j.min(n)];
            i = j + 1;
            let mut k = i;
            while k < n && (b[k] == b' ' || b[k] == b'\t' || b[k] == b'\n' || b[k] == b'\r') {
                k += 1;
            }
            if k < n && b[k] == b':' {
                pending = Some(lit);
                i = k + 1;
            } else if let Some(key) = pending.take() {
                pairs.push((key, lit));
            }
            continue;
        }
        if matches!(b[i], b',' | b'{' | b'}' | b'[' | b']') {
            pending = None;
        }
        i += 1;
    }
    pairs
}

/// 帶有罕見類別值的元素索引（升冪、已去重）。
fn rare_value_indices(elem_texts: &[&str]) -> Vec<usize> {
    // BTreeMap 保證鍵有序 → 兩語言迭代順序一致
    let mut by_key: BTreeMap<&str, BTreeMap<&str, Vec<usize>>> = BTreeMap::new();
    for (idx, t) in elem_texts.iter().enumerate() {
        for (key, val) in json_string_pairs(t) {
            by_key.entry(key).or_default().entry(val).or_default().push(idx);
        }
    }
    let mut rare: BTreeSet<usize> = BTreeSet::new();
    for (_key, values) in by_key {
        if values.len() < 2 || values.len() > RARE_MAX_CARDINALITY {
            continue;
        }
        let total: usize = values.values().map(|v| v.len()).sum();
        // 頻率降冪；同頻以值字串升冪 → 與 Python sorted(key=(-len, value)) 一致
        let mut ordered: Vec<(&str, &Vec<usize>)> = values.iter().map(|(k, v)| (*k, v)).collect();
        ordered.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
        let mut cum = 0usize;
        let mut k_needed = 0usize;
        for (rank, (_, idxs)) in ordered.iter().enumerate() {
            cum += idxs.len();
            if cum * 100 >= total * RARE_COVERAGE_PCT {
                k_needed = rank + 1;
                break;
            }
        }
        if k_needed == 0 || k_needed > RARE_MAX_K {
            continue;
        }
        for (_, idxs) in &ordered[k_needed..] {
            rare.extend(idxs.iter().copied());
        }
    }
    rare.into_iter().collect()
}

/// 純函式：找元素最多的 array，保留頭+尾+罕見值元素，其餘丟棄。壓不動回 `None`。
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
    let total = elems.len();
    let elem_texts: Vec<&str> = elems.iter().map(|&(s, e)| &text[s..e]).collect();

    let mut keep: BTreeSet<usize> = BTreeSet::new();
    for i in 0..JSON_HEAD.min(total) {
        keep.insert(i);
    }
    for i in total.saturating_sub(JSON_TAIL)..total {
        keep.insert(i);
    }
    // 罕見元素只從「原本會被丟掉」的那些裡挑；聯集上限為總數的 RARE_MAX_KEEP_PCT %
    let rare_cap = total * RARE_MAX_KEEP_PCT / 100;
    let rare: Vec<usize> = rare_value_indices(&elem_texts)
        .into_iter()
        .filter(|i| !keep.contains(i))
        .take(rare_cap)
        .collect();
    keep.extend(rare);

    if total.saturating_sub(keep.len()) < MIN_JSON_DROP {
        return None;
    }

    let digest = content_key(text);
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < total {
        if keep.contains(&i) {
            parts.push(elem_texts[i].to_string());
            i += 1;
            continue;
        }
        let mut j = i;
        while j < total && !keep.contains(&j) {
            j += 1;
        }
        // 每段連續丟棄各插一個 marker；無罕見元素時只有一段 → 與 M22 前逐字相同
        parts.push(format!(
            "\"[... headroom-lite dropped {} array elements | sha256:{digest} ...]\"",
            j - i
        ));
        i = j;
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

// ── M16 — stack trace 內容感知策略（Rust port，對齊 Python `strategies.py`）──
//
// 對象：遞迴爆炸 / 深框架的 stack trace（Python `File "..."`、Java/JS `at ...(`）。把 trace
// 切成 frame，保前 STACK_KEEP_HEAD + 後 STACK_KEEP_TAIL 個 frame、丟中段 frame（連同續行），
// 非 frame 行（`Traceback` 標頭、最終 `XxxError: msg`、chained-exception 分隔）一律保留。
//
// 與盲目頭尾截斷的差別：truncate 以「行」為單位會切半個 frame，且尾端多行非 frame 訊息時
// 可能擠掉最關鍵的錯誤訊息行；stack 策略以 frame 為邊界、永不切半、非 frame 訊號行恆保留。
//
// 與 log/diff/search 對稱、全用 byte 級判別（skip 0x20/0x09 前綴後比對 `File "` / `at `），
// 不走 trim —— 避開 Python unicode 空白與 Rust trim 分岔。frame 切段與丟棄純靠 index 數學
// （保序逐行掃），無雜湊順序依賴 → Py/Rs 逐字節一致。
//
// 收存點不對稱承襲 M11–M15：Rust squeeze 純函式回 Option、put 在 compress_block 呼叫端。
// 註冊排在 LOG 之後、TRUNCATE 之前：既有 fixture 由前面策略先接走，stack 只接純 stack trace。

const MIN_STACK_LINES: usize = 8;
const MIN_STACK_FRAMES: usize = 10;
const STACK_KEEP_HEAD: usize = 3;
const STACK_KEEP_TAIL: usize = 3;
const STACK_DROP_RATIO: f64 = 0.3;

/// 只去除前綴 ASCII 空白（0x20/0x09），與 Python `_strip_ascii_ws` 逐字節對齊。
fn strip_ascii_ws(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    &line[i..]
}

/// frame 標頭判斷：去 ASCII 前綴空白後，像 Python `File "..."` 或 Java/JS `at ...(...)`。
/// `at ` 額外要求含 `(` —— 真 frame 帶 `(File:line)`，藉此擋掉 "at the store" 這類 prose。
fn is_frame_header(line: &str) -> bool {
    let s = strip_ascii_ws(line);
    if s.starts_with("File \"") {
        return true;
    }
    s.starts_with("at ") && s.contains('(')
}

/// frame 續行：以 ASCII 空白（空格/tab）起頭的非空行（如 Python frame 下的程式碼行）。
fn is_continuation(line: &str) -> bool {
    matches!(line.as_bytes().first(), Some(b' ') | Some(b'\t'))
}

/// 把行序列切成 frame 區段，回各 frame 的 (start, end_exclusive) 行索引範圍。
/// frame = 標頭行 + 其後續行（縮排、非標頭），直到下一個標頭行或非續行為止。
fn segment_frames(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut frames: Vec<(usize, usize)> = Vec::new();
    let n = lines.len();
    let mut i = 0;
    while i < n {
        if is_frame_header(lines[i]) {
            let start = i;
            i += 1;
            while i < n && !is_frame_header(lines[i]) && is_continuation(lines[i]) {
                i += 1;
            }
            frames.push((start, i));
        } else {
            i += 1;
        }
    }
    frames
}

/// 回「應丟棄」的行 drop flags：中段 frame（保前 HEAD + 後 TAIL）的所有行。
/// frame 數不足 → 全 false。純 index 數學、保序 → 確定性、parity 友善。
fn stack_drop_flags(lines: &[&str]) -> Vec<bool> {
    let mut flags = vec![false; lines.len()];
    let frames = segment_frames(lines);
    if frames.len() < MIN_STACK_FRAMES {
        return flags;
    }
    let last = frames.len() - STACK_KEEP_TAIL;
    for &(start, end) in &frames[STACK_KEEP_HEAD..last] {
        for f in flags.iter_mut().take(end).skip(start) {
            *f = true;
        }
    }
    flags
}

/// 嗅探：夠多行、frame 數足、且中段可丟行佔比夠高才認領；否則讓 truncate 兜底。
fn stack_applies(text: &str) -> bool {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() < MIN_STACK_LINES {
        return false;
    }
    let dropped = stack_drop_flags(&lines).iter().filter(|d| **d).count();
    if dropped == 0 {
        return false;
    }
    (dropped as f64 / lines.len() as f64) >= STACK_DROP_RATIO
}

/// 保前 HEAD + 後 TAIL 個 frame、丟中段 frame，非 frame 行全留，丟棄處塞單一 marker。
/// 沒可丟 frame 回 `None`（呼叫端保留原文、不 put）。marker 含丟掉 frame 數 + content_key。
fn stack_squeeze(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let flags = stack_drop_flags(&lines);
    if !flags.iter().any(|d| *d) {
        return None;
    }
    let frames = segment_frames(&lines);
    let dropped_frames = frames.len() - STACK_KEEP_HEAD - STACK_KEEP_TAIL;
    let marker = format!(
        "[... headroom-lite dropped {dropped_frames} stack frames | sha256:{} ...]",
        content_key(text)
    );
    let mut parts: Vec<&str> = Vec::new();
    let mut marker_emitted = false;
    for (line, drop) in lines.iter().zip(&flags) {
        if *drop {
            if !marker_emitted {
                parts.push(&marker); // 中段第一個丟棄處塞一次 marker，其餘丟棄行省略
                marker_emitted = true;
            }
            continue;
        }
        parts.push(line);
    }
    Some(parts.join("\n"))
}

/// stacktrace 排在 log 之後、truncate 之前：純 stack trace（無 INFO/DEBUG 噪音）不被 log
/// 認領 → 落到此；既有 log/diff/search/json fixture 已由前面策略接走，stack 不回歸它們。
pub const STACKTRACE: Strategy = Strategy {
    name: "stacktrace",
    applies: stack_applies,
    squeeze: stack_squeeze,
};

// ── M17 — CSV/表格 內容感知策略（Rust port，對齊 Python `strategies.py`）──
//
// 對象：CSV/TSV 等表格輸出（DB 查詢結果、`column` 對齊匯出、資料表 dump）。噪音 = 大量
// 同構資料列；訊號 = 表頭（欄名）+ 頭尾代表性資料列。壓法：保表頭 + 前 CSV_KEEP_HEAD +
// 後 CSV_KEEP_TAIL 列，中段以單一 marker 取代（CCR store 保原文可逆）。
//
// 與盲目頭尾截斷的差別：truncate 不理解「表頭=訊號」的語意——資料列多到把表頭擠出 HEAD_LINES
// 視窗時欄名就此遺失；CSV 策略明確把表頭釘在輸出第一行、再配頭尾代表列。
//
// 嗅探（保守、強訊號防誤判）：去單一尾端換行後，**每一非空行**都以同一 delimiter（`,` 優先、
// 再 `\t`）出現「相同次數且 >= 1」才認領 —— 散文不可能每行逗號數一致。含內部空行 → 不認領。
// 引號內逗號破壞「每行同數」而自動落 truncate 兜底（保守）。delimiter 計數純 ASCII byte
// （`,`=0x2C / `\t`=0x09 皆非 UTF-8 續位元組）→ 與 Python str.count 逐字節一致。
//
// 收存點不對稱承襲 M11–M16：Rust squeeze 純函式回 Option、put 在 compress_block 呼叫端。

const MIN_CSV_LINES: usize = 8;
const CSV_KEEP_HEAD: usize = 3;
const CSV_KEEP_TAIL: usize = 2;
const MIN_CSV_DROP: usize = 4;

/// 某 ASCII byte 在一行中的出現次數（純 byte，對齊 Python `str.count` 對單 ASCII 字元）。
fn byte_count(line: &str, b: u8) -> usize {
    line.bytes().filter(|&x| x == b).count()
}

/// 判斷是否為乾淨表格：回 clean 行序列（含表頭）或 `None`。
/// 條件：去單一尾端換行後行數 >= MIN_CSV_LINES、無內部空行、且某 delimiter（`,` 優先、
/// 再 `\t`）讓每行出現次數相同且 >= 1。
fn csv_rows(text: &str) -> Option<Vec<&str>> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop(); // 容忍單一尾端換行
    }
    if lines.len() < MIN_CSV_LINES {
        return None;
    }
    if lines.iter().any(|l| l.is_empty()) {
        return None; // 內部空行 → 非乾淨表格
    }
    for delim in [b',', b'\t'] {
        let c0 = byte_count(lines[0], delim);
        if c0 >= 1 && lines.iter().all(|l| byte_count(l, delim) == c0) {
            return Some(lines);
        }
    }
    None
}

/// 純函式：保表頭 + 頭尾資料列、中段塞 marker。壓不動回 `None`。
fn csv_squeeze_core(text: &str) -> Option<String> {
    let lines = csv_rows(text)?;
    let data_rows = lines.len() - 1;
    // 提前擋（同時避免 usize 下溢）：可丟列數不足 → None。
    if data_rows < CSV_KEEP_HEAD + CSV_KEEP_TAIL + MIN_CSV_DROP {
        return None;
    }
    let dropped = data_rows - CSV_KEEP_HEAD - CSV_KEEP_TAIL;
    let marker = format!(
        "[... headroom-lite dropped {dropped} table rows | sha256:{} ...]",
        content_key(text)
    );
    let mut parts: Vec<&str> = Vec::with_capacity(1 + CSV_KEEP_HEAD + 1 + CSV_KEEP_TAIL);
    parts.push(lines[0]); // 表頭恆保留
    parts.extend(&lines[1..1 + CSV_KEEP_HEAD]); // 前 head 資料列
    parts.push(&marker);
    parts.extend(&lines[lines.len() - CSV_KEEP_TAIL..]); // 後 tail 資料列
    Some(parts.join("\n"))
}

/// 嗅探：是乾淨表格、且中段可丟列數夠多（>= MIN_CSV_DROP）才認領。
fn csv_applies(text: &str) -> bool {
    csv_squeeze_core(text).is_some()
}

/// 保表頭 + 頭尾列；壓不動回 `None`（呼叫端保留原文、不 put）。
fn csv_squeeze(text: &str) -> Option<String> {
    csv_squeeze_core(text)
}

/// csv 排在 stacktrace 之後、truncate 之前：表格無 JSON/diff/search/log/frame 結構 → 不被前面
/// 策略認領、落到此；既有 fixture 已由前面策略接走，csv 只接純表格 → 零回歸。
pub const CSV: Strategy = Strategy {
    name: "csv",
    applies: csv_applies,
    squeeze: csv_squeeze,
};

// ── M18 — Markdown table 內容感知策略（Rust port，對齊 Python `strategies.py`）──
//
// 對象：GitHub-flavored markdown 表格（LLM 輸出、文件、README 裡極常見）。噪音 = 大量同構
// 資料列；訊號 = 表頭（欄名）+ **分隔列 `|---|---|`**（定義欄位對齊、合法 markdown 表格的必要
// 結構）+ 頭尾代表性資料列。壓法：保表頭 + 分隔列 + 前 MD_KEEP_HEAD + 後 MD_KEEP_TAIL 列，
// 中段以單一 marker 取代（CCR store 保原文可逆）。
//
// 與 CSV（M17）的差別（不只換 delimiter）：markdown 表格多一條「分隔列」必須釘住保留 ——
// truncate 以行為單位會把表頭與分隔列一起擠出視窗；本策略明確把兩者釘在輸出最前。
//
// 嗅探（保守、強訊號防誤判）：去單一尾端換行後行數 >= MIN_MD_LINES、無內部空行、每行含相同
// 數量（>= 1）的 `|`，且第二行是合法分隔列（只由 `|` `:` `-` 空白組成且至少一個 `-`）。分隔列
// 是與 CSV/散文的關鍵鑑別子。pipe/分隔列計數純 ASCII byte（`|`=0x7C / `-`=0x2D / `:`=0x3A 皆非
// UTF-8 續位元組）→ 與 Python str.count 逐字節一致、Py/Rs parity。
//
// 收存點不對稱承襲 M11–M17：Rust squeeze 純函式回 Option、put 在 compress_block 呼叫端。

const MIN_MD_LINES: usize = 8;
const MD_KEEP_HEAD: usize = 3;
const MD_KEEP_TAIL: usize = 2;
const MIN_MD_DROP: usize = 4;

/// markdown 表格分隔列判斷：只由 `|` `:` `-` 空白組成、且至少含一個 `-`。
/// 純 ASCII byte 檢查（非 ASCII 字元的 byte 皆 >127、不在允許集 → 自動排除）。
fn is_md_separator(line: &str) -> bool {
    let bs = line.as_bytes();
    let has_dash = bs.contains(&b'-');
    has_dash && bs.iter().all(|&b| matches!(b, b'|' | b':' | b'-' | b' '))
}

/// 判斷是否為乾淨 markdown 表格：回 clean 行序列（含表頭、分隔列）或 `None`。
/// 條件：去單一尾端換行後行數 >= MIN_MD_LINES、無內部空行、每行 `|` 數相同且 >= 1、
/// 第二行是合法分隔列。`|` 計數純 ASCII byte（沿用 byte_count）。
fn md_rows(text: &str) -> Option<Vec<&str>> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop(); // 容忍單一尾端換行
    }
    if lines.len() < MIN_MD_LINES {
        return None;
    }
    if lines.iter().any(|l| l.is_empty()) {
        return None; // 內部空行 → 非乾淨表格
    }
    let c0 = byte_count(lines[0], b'|');
    if c0 < 1 || lines.iter().any(|l| byte_count(l, b'|') != c0) {
        return None; // 每行 pipe 數須一致且 >= 1
    }
    if !is_md_separator(lines[1]) {
        return None; // 第二行須為分隔列 —— 與 CSV/散文的關鍵鑑別子
    }
    Some(lines)
}

/// 純函式：保表頭 + 分隔列 + 頭尾資料列、中段塞 marker。壓不動回 `None`。
fn md_squeeze_core(text: &str) -> Option<String> {
    let lines = md_rows(text)?;
    let data_rows = lines.len() - 2; // 扣除表頭 + 分隔列
    // 提前擋（同時避免 usize 下溢）：可丟列數不足 → None。
    if data_rows < MD_KEEP_HEAD + MD_KEEP_TAIL + MIN_MD_DROP {
        return None;
    }
    let dropped = data_rows - MD_KEEP_HEAD - MD_KEEP_TAIL;
    let marker = format!(
        "[... headroom-lite dropped {dropped} markdown table rows | sha256:{} ...]",
        content_key(text)
    );
    let mut parts: Vec<&str> = Vec::with_capacity(2 + MD_KEEP_HEAD + 1 + MD_KEEP_TAIL);
    parts.push(lines[0]); // 表頭恆保留
    parts.push(lines[1]); // 分隔列恆保留（結構訊號）
    parts.extend(&lines[2..2 + MD_KEEP_HEAD]); // 前 head 資料列
    parts.push(&marker);
    parts.extend(&lines[lines.len() - MD_KEEP_TAIL..]); // 後 tail 資料列
    Some(parts.join("\n"))
}

/// 嗅探：是乾淨 markdown 表格、且中段可丟列數夠多（>= MIN_MD_DROP）才認領。
fn md_applies(text: &str) -> bool {
    md_squeeze_core(text).is_some()
}

/// 保表頭 + 分隔列 + 頭尾列；壓不動回 `None`（呼叫端保留原文、不 put）。
fn md_squeeze(text: &str) -> Option<String> {
    md_squeeze_core(text)
}

/// markdown 排在 stacktrace 之後、csv 之前：markdown 表格（pipe + 分隔列）比逗號 CSV 更專一，
/// 兩者其實互斥（pipe vs 逗號）——markdown 先嗅探保證真 markdown 表格不被 csv 誤搶；既有 csv
/// fixture 無 pipe → markdown 不認領、零回歸。
pub const MARKDOWN: Strategy = Strategy {
    name: "markdown",
    applies: md_applies,
    squeeze: md_squeeze,
};

// ── M19 — base64/hex blob 內容感知策略（Rust port，對齊 Python `strategies.py`）──
//
// 對象：單行巨型編碼 blob（data URI 內嵌圖片、base64 編碼附件、長 hex dump、JWT 等）。中段
// 對推理是不透明噪音，保頭尾足以辨識、中段交 CCR store 可逆取回。
//
// ⭐ 與前七片的根本差別：第一片「字元範圍（intra-line）」策略 —— 前七片全是行級/array 元素級。
// 找最長的「連續 blob 字元串」（base64/base64url/hex 字元集，不含換行/空白 = 單一 token），保前
// BLOB_HEAD + 後 BLOB_TAIL 字元、中段塞 marker，串外 bytes 照抄。
//
// ⭐ parity 正解：字元範圍切片在 Python（依 code point）與 Rust（依 byte）天生分岔 —— 解法是
// **要求整段 text 為純 ASCII**（`is_ascii`）。ASCII 下 code point 與 byte 一對一，兩語言切片偏移
// 完全一致；非 ASCII 一律不認領、落 truncate 兜底。blob 本就純 ASCII。
//
// 嗅探（保守、強訊號防誤判）：連續 blob 字元串（無空白/換行/標點打斷）須 >= MIN_BLOB_RUN；散文
// 不可能有 512 字元不含空白的連續串。tie-break 取最長、同長取最前（嚴格大於才替換）→ Py/Rs 一致。
// 限制：只認單行 blob（run 不跨換行），MIME/PEM 多行折疊留作未來擴充。
//
// 收存點不對稱承襲 M11–M17：Rust squeeze 純函式回 Option、put 在 compress_block 呼叫端。

const MIN_BLOB_RUN: usize = 512;
const BLOB_HEAD: usize = 64;
const BLOB_TAIL: usize = 64;

/// base64 / base64url / hex 字元集：ASCII 英數 + `+` `/` `=` `_` `-`（純 byte，parity 安全）。
fn is_blob_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-')
}

/// 回最長連續 blob 字元串的 (start, end)；無則 (0, 0)。同長取最前（嚴格大於才替換）。
fn longest_blob_run(data: &[u8]) -> (usize, usize) {
    let (mut best_s, mut best_e) = (0usize, 0usize);
    let n = data.len();
    let mut i = 0;
    while i < n {
        if is_blob_char(data[i]) {
            let start = i;
            while i < n && is_blob_char(data[i]) {
                i += 1;
            }
            if (i - start) > (best_e - best_s) {
                best_s = start;
                best_e = i;
            }
        } else {
            i += 1;
        }
    }
    (best_s, best_e)
}

/// 純函式：找最長 blob 串、保頭尾字元、中段塞 marker。壓不動回 `None`。
/// 非 ASCII 一律回 None —— 字元範圍切片需 byte == char 才能 Py/Rs 一致。
fn blob_squeeze_core(text: &str) -> Option<String> {
    if !text.is_ascii() {
        return None;
    }
    let data = text.as_bytes(); // ASCII 下 byte index == char index → 切片偏移兩語言一致
    let (start, end) = longest_blob_run(data);
    let run_len = end - start;
    if run_len < MIN_BLOB_RUN {
        // MIN_BLOB_RUN > HEAD+TAIL → dropped 必為正、無 usize 下溢
        return None;
    }
    let dropped = run_len - BLOB_HEAD - BLOB_TAIL;
    let marker = format!(
        "[... headroom-lite dropped {dropped} blob chars | sha256:{} ...]",
        content_key(text)
    );
    Some(format!(
        "{}{}{}{}{}",
        &text[..start],
        &text[start..start + BLOB_HEAD],
        marker,
        &text[end - BLOB_TAIL..end],
        &text[end..],
    ))
}

/// 嗅探：純 ASCII、且最長連續 blob 串 >= MIN_BLOB_RUN 才認領。
fn blob_applies(text: &str) -> bool {
    blob_squeeze_core(text).is_some()
}

/// 保 blob 頭尾字元；壓不動回 `None`（呼叫端保留原文、不 put）。
fn blob_squeeze(text: &str) -> Option<String> {
    blob_squeeze_core(text)
}

/// blob 排在 csv 之後、truncate 之前：極專一（需 512 字元連續 blob 串 + 純 ASCII）排最末才安全；
/// 單行 blob 無換行 → 多行策略全不認領，blob 接住「否則落 truncate 卻因單行無法壓」的巨型 blob。
pub const BLOB: Strategy = Strategy {
    name: "blob",
    applies: blob_applies,
    squeeze: blob_squeeze,
};

// ── M20 — HTML/XML 內容感知策略（Rust port，對齊 Python `strategies.py`）──
//
// 對象：HTML/XML 文件（網頁爬取）。噪音 = `<script>`/`<style>` 內文（巨型 inline JS/CSS）+
// `<!-- -->` 註解；訊號 = 標籤結構與可見文字。壓法：保留每個噪音區的邊界（開閉標籤 / 註解
// `<!--` `-->`），把內文換成單一 marker（CCR store 可逆）。
//
// ⭐ parity（沿用 M15 JSON 模式，非 ASCII 安全）：Rust 用 **byte index** find/slice、Python 用
// **char index**，各自原生索引定位同一邏輯位置 → 切出的邏輯子字串相同、輸出 bytes 一致。標籤名
// 只比對小寫（避開 unicode lower() 改變長度的 index 陷阱）。切點都在 ASCII 標籤邊界 → 不破 UTF-8。
//
// 收存點不對稱承襲 M11–M19：Rust squeeze 純函式回 Option、put 在 compress_block 呼叫端。

const MIN_HTML_NOISE: usize = 256;
const HTML_NOISE_TAGS: [&str; 2] = ["script", "style"];

/// 從 byte 位置 `from` 起找 `pat`，回絕對 byte 位置或 `None`。
fn find_from(text: &str, pat: &str, from: usize) -> Option<usize> {
    text[from..].find(pat).map(|p| from + p)
}

/// 回所有「可挖」噪音區的 (inner_start, inner_end)（byte offset），保序、不重疊、內文 >= MIN_HTML_NOISE。
fn html_noise_regions(text: &str) -> Vec<(usize, usize)> {
    let mut regions: Vec<(usize, usize)> = Vec::new();
    let n = text.len();
    let mut i = 0;
    while i < n {
        // 找最早出現的噪音開頭：<script / <style / <!--
        let mut best_pos: Option<usize> = None;
        let mut best_kind = "";
        for tag in HTML_NOISE_TAGS {
            let needle = format!("<{tag}");
            if let Some(p) = find_from(text, &needle, i) {
                if best_pos.is_none_or(|b| p < b) {
                    best_pos = Some(p);
                    best_kind = tag;
                }
            }
        }
        if let Some(pc) = find_from(text, "<!--", i) {
            if best_pos.is_none_or(|b| pc < b) {
                best_pos = Some(pc);
                best_kind = "<!--";
            }
        }
        let Some(pos) = best_pos else { break };

        let (inner_start, inner_end, nxt);
        if best_kind == "<!--" {
            inner_start = pos + 4; // len("<!--")
            let Some(close) = find_from(text, "-->", inner_start) else {
                break; // 未終結註解 → 停（保守）
            };
            inner_end = close;
            nxt = close + 3;
        } else {
            let Some(gt) = find_from(text, ">", pos) else {
                break; // 開標籤未閉合
            };
            inner_start = gt + 1;
            let closer = format!("</{best_kind}");
            let Some(close) = find_from(text, &closer, inner_start) else {
                i = gt + 1; // 找不到閉標籤 → 跳過此開頭、保證前進
                continue;
            };
            inner_end = close;
            nxt = close; // 從閉標籤處續掃（`</tag` 不會誤配 `<tag`）
        }

        if inner_end - inner_start >= MIN_HTML_NOISE {
            regions.push((inner_start, inner_end));
        }
        i = if nxt > i { nxt } else { i + 1 }; // 保證前進
    }
    regions
}

/// 純函式：把每個噪音區的內文換成 marker、保留邊界與結構。無可挖區回 `None`。
fn html_squeeze_core(text: &str) -> Option<String> {
    let regions = html_noise_regions(text);
    if regions.is_empty() {
        return None;
    }
    let digest = content_key(text);
    let mut out = String::new();
    let mut prev = 0;
    for (start, end) in regions {
        out.push_str(&text[prev..start]); // 邊界 + 結構（含非 ASCII 文字）逐字保留
        let dropped = end - start;
        out.push_str(&format!(
            "[... headroom-lite dropped {dropped} html noise chars | sha256:{digest} ...]"
        ));
        prev = end;
    }
    out.push_str(&text[prev..]);
    Some(out)
}

/// 嗅探：存在至少一個內文 >= MIN_HTML_NOISE 的 script/style/comment 噪音區才認領。
fn html_applies(text: &str) -> bool {
    html_squeeze_core(text).is_some()
}

/// 挖掉 script/style/comment 內文；無可挖回 `None`（呼叫端保留原文、不 put）。
fn html_squeeze(text: &str) -> Option<String> {
    html_squeeze_core(text)
}

/// html 排在 csv 之後、blob 之前：含 inline script 的頁面該走 HTML（保結構）而非被 blob 當巨串
/// 吞掉；data URI 無 `<script`/`<style`/`<!--` → HTML 不認領、落 blob。既有 fixture 皆無噪音區
/// → HTML 不認領、零回歸。
pub const HTML: Strategy = Strategy {
    name: "html",
    applies: html_applies,
    squeeze: html_squeeze,
};

/// 策略註冊表：按優先序排列。json/diff/search/log/stacktrace/markdown/csv/html/blob 先嗅探，不命中才落 truncate 兜底。
pub const STRATEGIES: &[Strategy] =
    &[JSON, DIFF, SEARCH, LOG, STACKTRACE, MARKDOWN, CSV, HTML, BLOB, TRUNCATE];

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
