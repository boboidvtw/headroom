//! M23 — volatile 內容唯讀掃描（Phase E 的 observe 那一半）。
//!
//! 與 Python `headroom_lite/volatile.py` 逐項對齊。完整的設計理由、三處
//! 刻意偏離解答本之處、以及 policy 都寫在 Python 那份 module docstring；
//! 這裡只記 Rust 特有的細節。
//!
//! # 非變性不變量
//!
//! 入口收 `&[u8]`、自己 parse 一份 `Value`。工業版收 `&Value`（省一次
//! parse），非變性靠呼叫端的 `debug_assert_eq!` 與整合測試守；重建多付
//! 一次 parse 換取「掃描器手上根本沒有呼叫端的物件」—— 借用檢查器讓
//! 違反非變性連編譯都過不了，不必靠測試守。
//!
//! # 索引範式（M15/M20 native-index）
//!
//! 這裡逐 **byte** 掃，Python 端逐 **code point** 掃。索引不同，但兩個
//! pattern 都是純 ASCII：認出的是同一個子字串、跳過的是同一段
//! （19 個 ASCII 字元 == 19 bytes），而且只回報子字串本身、從不回報偏移量。
//! 非 ASCII 位置的 byte 一定不是 ASCII 數字/hex，不可能誤匹配，也因此
//! `&s[i..i + LEN]` 的兩端在命中時必然落在 char boundary 上。

use serde_json::Value;

/// 每請求回報上限。客戶貼一份 CSV 進 system prompt 就能產出幾百條警告。
pub const MAX_FINDINGS: usize = 10;

/// sample 上限，以**字元**計（不是 byte —— 見 Python 版的偏離說明 b）。
pub const SAMPLE_MAX_CHARS: usize = 80;

/// 慣例上「每請求唯一」的 JSON key 名，對 key 做 ASCII 小寫後子字串比對。
const ID_FIELD_NEEDLES: &[&str] = &["request_id", "trace_id", "session_id", "correlation_id"];

const ISO_LEN: usize = 19;
const UUID_LEN: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolatileKind {
    /// `YYYY-MM-DDTHH:MM:SS`：4=`-`、7=`-`、10=`T`/`t`/空格、13=`:`、16=`:`。
    Timestamp,
    /// UUID v4：36 字元、hex、`-` 在 8/13/18/23、version nibble 14 是 `4`。
    Uuid,
    /// key 名含慣例上每請求唯一的 ID needle。
    IdField,
}

impl VolatileKind {
    /// 穩定的字串表示（parity 報告與觀測線都吃它），別隨手改。
    pub fn as_str(self) -> &'static str {
        match self {
            VolatileKind::Timestamp => "iso8601_timestamp",
            VolatileKind::Uuid => "uuid_v4",
            VolatileKind::IdField => "id_field",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolatileFinding {
    pub kind: VolatileKind,
    /// JSON-pointer 風格路徑，例如 `tools[0].input_schema.properties.session_id`。
    pub location: String,
    pub sample: String,
}

// ─── 入口 ──────────────────────────────────────────────────────────────

/// 掃描 request body bytes。永不修改任何東西、永不 panic。
///
/// 壞輸入（非 JSON / 非 object）回空 Vec —— 與 M0 起的失敗模式契約一致。
pub fn scan_request(raw: &[u8]) -> Vec<VolatileFinding> {
    let Ok(body) = serde_json::from_slice::<Value>(raw) else {
        return Vec::new();
    };
    if !body.is_object() {
        return Vec::new();
    }
    detect_volatile_content(&body)
}

/// 走訪 Anthropic `/v1/messages` 形狀的快取熱區。唯讀（`&Value`）。
///
/// 走訪順序固定（system → messages → tools），object key 依插入順序
/// （crate 開了 `preserve_order`）—— 兩邊撞到上限時砍掉的是同一批。
pub fn detect_volatile_content(body: &Value) -> Vec<VolatileFinding> {
    let mut out: Vec<VolatileFinding> = Vec::new();

    if let Some(system) = body.get("system") {
        scan_content(system, "system", &mut out);
    }

    if let Some(Value::Array(messages)) = body.get("messages") {
        // **最後一則不掃**（刻意偏離解答本的 E5，它掃全部 messages）。
        //
        // 快取前綴的邊界由 M3 的 `_place_breakpoints` 定義：標記 2 放在
        // `messages[-2]`，所以最後一則從來就不在前綴裡 —— 那是 live zone，
        // 它每輪都變、變了無害，也正是壓縮引擎接著要改寫的東西。
        //
        // 上限是全域的、走訪順序是 system → messages → tools，光一則塞滿
        // 時間戳的 tool_result 就能灌滿 10 筆，把 tools 裡真正該報的東西
        // 安靜擠掉 —— 噪音之外還會漏報。詳見 Python 版同段註解。
        let frozen = messages.len().saturating_sub(1);
        for (i, message) in messages[..frozen].iter().enumerate() {
            if out.len() >= MAX_FINDINGS {
                return out;
            }
            if let Some(content) = message.get("content") {
                scan_content(content, &format!("messages[{i}].content"), &mut out);
            }
        }
    }

    if let Some(Value::Array(tools)) = body.get("tools") {
        for (i, tool) in tools.iter().enumerate() {
            if out.len() >= MAX_FINDINGS {
                return out;
            }
            if let Some(Value::String(description)) = tool.get("description") {
                scan_string(description, &format!("tools[{i}].description"), &mut out);
            }
            if let Some(schema) = tool.get("input_schema") {
                scan_value(schema, &format!("tools[{i}].input_schema"), &mut out);
            }
        }
    }

    out
}

// ─── 走訪 ──────────────────────────────────────────────────────────────

/// content 位置：可能是字串、可能是 block 陣列、也可能是 object。
fn scan_content(value: &Value, location: &str, out: &mut Vec<VolatileFinding>) {
    if out.len() >= MAX_FINDINGS {
        return;
    }
    match value {
        Value::String(s) => scan_string(s, location, out),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if out.len() >= MAX_FINDINGS {
                    return;
                }
                scan_value(item, &format!("{location}[{i}]"), out);
            }
        }
        Value::Object(_) => scan_value(value, location, out),
        _ => {}
    }
}

/// 唯一會檢查 **key 名稱** 的走訪器。
fn scan_value(value: &Value, location: &str, out: &mut Vec<VolatileFinding>) {
    if out.len() >= MAX_FINDINGS {
        return;
    }
    match value {
        Value::String(s) => scan_string(s, location, out),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if out.len() >= MAX_FINDINGS {
                    return;
                }
                scan_value(item, &format!("{location}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            for (key, sub) in map.iter() {
                if out.len() >= MAX_FINDINGS {
                    return;
                }
                if is_id_named_key(key) && !is_value_empty(sub) {
                    out.push(VolatileFinding {
                        kind: VolatileKind::IdField,
                        location: format!("{location}.{key}"),
                        sample: truncate_sample(&value_to_sample(sub)),
                    });
                    if out.len() >= MAX_FINDINGS {
                        return;
                    }
                }
                scan_value(sub, &format!("{location}.{key}"), out);
            }
        }
        _ => {}
    }
}

/// 在一段字串裡找時間戳與 UUID v4。同一段字串裡多次命中各記一筆。
fn scan_string(text: &str, location: &str, out: &mut Vec<VolatileFinding>) {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0usize;
    while i < n {
        if out.len() >= MAX_FINDINGS {
            return;
        }
        // 先試 ISO-8601：視窗較短，字串剛好在 UUID 中間結束時比較不會漏。
        if i + ISO_LEN <= n && looks_like_iso8601(&bytes[i..i + ISO_LEN]) {
            out.push(VolatileFinding {
                kind: VolatileKind::Timestamp,
                location: location.to_string(),
                sample: truncate_sample(&text[i..i + ISO_LEN]),
            });
            i += ISO_LEN;
            continue;
        }
        if i + UUID_LEN <= n && looks_like_uuid_v4(&bytes[i..i + UUID_LEN]) {
            out.push(VolatileFinding {
                kind: VolatileKind::Uuid,
                location: location.to_string(),
                sample: truncate_sample(&text[i..i + UUID_LEN]),
            });
            i += UUID_LEN;
            continue;
        }
        i += 1;
    }
}

// ─── pattern 判別（全部明寫位置，不用 regex）────────────────────────────

fn looks_like_iso8601(w: &[u8]) -> bool {
    let digits = |idx: &[usize]| idx.iter().all(|&k| w[k].is_ascii_digit());
    digits(&[0, 1, 2, 3])
        && w[4] == b'-'
        && w[7] == b'-'
        && digits(&[5, 6, 8, 9])
        && (w[10] == b'T' || w[10] == b't' || w[10] == b' ')
        && w[13] == b':'
        && w[16] == b':'
        && digits(&[11, 12, 14, 15, 17, 18])
}

fn looks_like_uuid_v4(w: &[u8]) -> bool {
    if w[8] != b'-' || w[13] != b'-' || w[18] != b'-' || w[23] != b'-' {
        return false;
    }
    if w[14] != b'4' {
        return false;
    }
    // variant nibble，RFC 4122 §4.4
    if !matches!(w[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B') {
        return false;
    }
    w.iter()
        .enumerate()
        .all(|(k, c)| matches!(k, 8 | 13 | 18 | 23) || c.is_ascii_hexdigit())
}

/// ASCII 小寫後的子字串比對。刻意用 `to_ascii_lowercase` 而非 unicode 版
/// —— needle 全是 ASCII，而 unicode 小寫可能改變長度（M20 踩過的雷），
/// Python 端也是自己折 A–Z 來對齊這一點。
fn is_id_named_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    ID_FIELD_NEEDLES.iter().any(|needle| lowered.contains(needle))
}

/// 空字串 / 空陣列 / 空物件 / null 視為「沒有值」——
/// 只是在 schema 裡『宣告』了 request_id 的 client 不該被指控。
fn is_value_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

fn value_to_sample(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        // arbitrary_precision 下 Number 的 to_string 就是原始字面值。
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// 以**字元**截斷。`chars().count()` 是 O(n)，但 sample 本來就短。
fn truncate_sample(s: &str) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(SAMPLE_MAX_CHARS).collect();
    if it.next().is_none() {
        return head;
    }
    format!("{head}…")
}
