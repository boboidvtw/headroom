//! M23 — volatile 內容唯讀掃描（Phase E 的 observe 那一半）。
//!
//! 與 Python `headroom_lite/volatile.py` 逐項對齊。完整的設計理由、sample
//! 政策、以及刻意偏離解答本之處都寫在 Python 那份 module docstring；
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
//!
//! # 輸入收斂的責任分工
//!
//! Rust 這側的「看不懂就回空」多半是 serde_json 免費給的（非 UTF-8、
//! `NaN`、落單 surrogate、巢狀 > 128 全部 parse 失敗）。**Python 那側沒有
//! 一項是免費的**，全都得手動對齊 —— 這正是 adversarial gate 存在的理由。

use serde_json::Value;

/// 相異 `(kind, location)` 的回報上限。上限算的是**位置**不是命中次數：
/// 同一段 log 裡的 40 個時間戳只佔一個名額，否則它會把 tools 裡真正該報
/// 的東西安靜擠掉。
pub const MAX_FINDINGS: usize = 10;

/// UUID sample 只留前綴，足以定位而不構成可用的憑證。
pub const UUID_SAMPLE_CHARS: usize = 8;

/// 單一 location 片段（客戶的 key 名）的字元上限；見 Python 版的完整說明。
/// 要點：location 是**客戶自己的 key 名**串起來的，祖先 key 完全不受 needle
/// 約束 —— 用 email 或 token 當 map key 的 JSON 會整串進觀測線。處置是設界
/// 而非消除，因為 location 是這筆 finding 唯一可行動的內容。
pub const MAX_LOCATION_SEGMENT_CHARS: usize = 40;
pub const MAX_LOCATION_CHARS: usize = 200;

/// 走訪深度上限，與 serde_json 的 parse 上限對齊。`scan_request` 走 parser
/// 就已受限，但 `detect_volatile_content` 是公開 API、收已建好的 `Value`
/// —— 沒有這道守門時實測會 stack overflow 而 **abort（exit 134，連攔都攔不到）**。
pub const MAX_DEPTH: usize = 127;

/// 超過這個大小就整包放棄掃描。這條路徑跑在**轉發之前**，掃多久就是延遲
/// 多久 —— 觀測是盡力而為的功能，不值得為一份 1 GB 的 body 佔住 worker。
pub const MAX_SCAN_BYTES: usize = 1 << 20;

/// 慣例上「每請求唯一」的 JSON key 名，對 key 做 ASCII 小寫後子字串比對。
/// 這是**開放集合**（`session_identity_token` 也命中）—— 正因為列舉不完，
/// sample 才不准回吐命中的值。
const ID_FIELD_NEEDLES: &[&str] = &["request_id", "trace_id", "session_id", "correlation_id"];

const ISO_LEN: usize = 19;
const UUID_LEN: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            VolatileKind::Timestamp => "iso8601_timestamp",
            VolatileKind::Uuid => "uuid_v4",
            VolatileKind::IdField => "id_field",
        }
    }
}

/// 一筆發現。`sample` 永不含 `IdField` 的值（見 Python 版的 sample 政策）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolatileFinding {
    pub kind: VolatileKind,
    /// 欄位存取路徑，例如 `tools[0].input_schema.properties.session_id`。
    pub location: String,
    pub sample: String,
    /// 同一個 `(kind, location)` 的命中次數。
    pub count: usize,
}

/// 掃描結果。`truncated` 為真代表「還有更多，我們放棄了」—— 沒有這個欄位，
/// 剛好 10 筆與撞上限長得一模一樣。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VolatileScan {
    pub findings: Vec<VolatileFinding>,
    /// 撞到 `MAX_FINDINGS`，還有更多相異位置沒列出。
    pub truncated: bool,
    /// body 超過 `MAX_SCAN_BYTES`，**根本沒掃**。與 `truncated` 是兩件事 ——
    /// 共用一個旗標會讓 proxy 對沒掃過的 body 印出「已達上限」這種事實錯誤
    /// 的觀測線（修掉第一層歧義又長出第二層）。
    pub skipped_too_large: bool,
}

// ─── 入口 ──────────────────────────────────────────────────────────────

/// 掃描 request body bytes。永不修改任何東西、永不 panic。
///
/// 壞輸入（非 JSON / 非 object / 巢狀過深）回空結果 —— 與 M0 起的失敗模式
/// 契約一致。
#[must_use]
pub fn scan_request(raw: &[u8]) -> VolatileScan {
    if raw.len() > MAX_SCAN_BYTES {
        return VolatileScan {
            findings: Vec::new(),
            truncated: false,
            skipped_too_large: true,
        };
    }
    // serde_json 一次擋掉四類：非 UTF-8、NaN/Infinity、落單 surrogate、
    // 巢狀 > 128 層。Python 那側四類都得手動對齊。
    let Ok(body) = serde_json::from_slice::<Value>(raw) else {
        return VolatileScan::default();
    };
    if !body.is_object() {
        return VolatileScan::default();
    }
    detect_volatile_content(&body)
}

/// 走訪 Anthropic `/v1/messages` 形狀的快取熱區。唯讀（`&Value`）。
///
/// 走訪順序固定（system → messages → tools），object key 依插入順序
/// （crate 開了 `preserve_order`）—— 兩邊撞到上限時砍掉的是同一批。
///
/// 注意：這是公開 API 且收**已建好的** `Value`，繞過了 `scan_request` 對
/// serde_json parse 深度上限的依賴。呼叫端若程式化建出極深的 `Value`，
/// 下面的遞迴會爆 stack —— 走 `scan_request` 就沒有這個問題。
#[must_use]
pub fn detect_volatile_content(body: &Value) -> VolatileScan {
    let mut out = Accumulator::new();

    if let Some(system) = body.get("system") {
        scan_content(system, &mut vec!["system".to_string()], &mut out, 1);
    }

    if let Some(Value::Array(messages)) = body.get("messages") {
        // **最後一則不掃**（刻意偏離解答本的 E5，它掃全部 messages）。
        //
        // 快取前綴的邊界由 M3 的 `_place_breakpoints` 定義：標記 2 放在
        // `messages[-2]`，所以最後一則從來就不在前綴裡 —— 那是 live zone，
        // 它每輪都變、變了無害，也正是壓縮引擎接著要改寫的東西。
        // 詳見 Python 版同段註解。
        let frozen = messages.len().saturating_sub(1);
        for (i, message) in messages[..frozen].iter().enumerate() {
            if out.full() {
                return out.finish();
            }
            if let Some(content) = message.get("content") {
                scan_content(
                    content,
                    &mut vec![format!("messages[{i}].content")],
                    &mut out,
                    1,
                );
            }
        }
    }

    if let Some(Value::Array(tools)) = body.get("tools") {
        for (i, tool) in tools.iter().enumerate() {
            if out.full() {
                return out.finish();
            }
            if let Some(Value::String(description)) = tool.get("description") {
                scan_string(
                    description,
                    &[format!("tools[{i}].description")],
                    &mut out,
                );
            }
            if let Some(schema) = tool.get("input_schema") {
                scan_value(
                    schema,
                    &mut vec![format!("tools[{i}].input_schema")],
                    &mut out,
                    1,
                );
            }
        }
    }

    out.finish()
}

/// 依 `(kind, location)` 去重的收集器。插入順序即輸出順序（與 Python 的
/// dict 一致）—— 不用 HashMap，雜湊順序會讓兩邊分岔。
struct Accumulator {
    entries: Vec<(VolatileKind, String, String, usize)>,
    truncated: bool,
}

impl Accumulator {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            truncated: false,
        }
    }

    fn add(&mut self, kind: VolatileKind, path: &[String], sample: String) {
        let location = render_location(path);
        if let Some(e) = self
            .entries
            .iter_mut()
            .find(|(k, loc, _, _)| *k == kind && *loc == location)
        {
            e.3 += 1;
            return;
        }
        if self.entries.len() >= MAX_FINDINGS {
            self.truncated = true;
            return;
        }
        self.entries.push((kind, location, sample, 1));
    }

    /// 撞上限後就停止走訪（別為了數重複而掃完整份 body）。
    fn full(&self) -> bool {
        self.truncated
    }

    fn finish(self) -> VolatileScan {
        VolatileScan {
            findings: self
                .entries
                .into_iter()
                .map(|(kind, location, sample, count)| VolatileFinding {
                    kind,
                    location,
                    sample,
                    count,
                })
                .collect(),
            truncated: self.truncated,
            skipped_too_large: false,
        }
    }
}

// ─── 走訪 ──────────────────────────────────────────────────────────────

/// content 位置：可能是字串、可能是 block 陣列、也可能是 object。
/// 行為與 `scan_value` 等價；保留兩個名字只為與解答本 / Python 端的結構對齊。
fn scan_content(value: &Value, path: &mut Vec<String>, out: &mut Accumulator, depth: usize) {
    if out.full() {
        return;
    }
    match value {
        Value::String(s) => scan_string(s, path, out),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if out.full() {
                    return;
                }
                path.push(format!("[{i}]"));
                scan_value(item, path, out, depth + 1);
                path.pop();
            }
        }
        Value::Object(_) => scan_value(value, path, out, depth),
        _ => {}
    }
}

/// 唯一會檢查 **key 名稱** 的走訪器。
///
/// `path` 是**可變的片段堆疊**，push/pop 而不是每個節點都串一條新字串。
/// 初版對每個 key、每個陣列元素都無條件 `format!` 一條 location，即使整份
/// body 一個 finding 都沒有 —— review 實測 1 MiB 零 findings 的深結構 body
/// 要 114 ms / 166 MB，同位元組數的淺結構只要 1.6 ms / 4.3 MB。
/// `MAX_SCAN_BYTES` 限的是位元組不是衍生工作量，那個洞正好從它底下鑽過去。
fn scan_value(value: &Value, path: &mut Vec<String>, out: &mut Accumulator, depth: usize) {
    if out.full() || depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::String(s) => scan_string(s, path, out),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if out.full() {
                    return;
                }
                path.push(format!("[{i}]"));
                scan_value(item, path, out, depth + 1);
                path.pop();
            }
        }
        Value::Object(map) => {
            for (key, sub) in map.iter() {
                if out.full() {
                    return;
                }
                path.push(format!(".{}", cap_segment(key)));
                if is_id_named_key(key) && !is_value_empty(sub) {
                    out.add(VolatileKind::IdField, path, describe(sub));
                }
                scan_value(sub, path, out, depth + 1);
                path.pop();
            }
        }
        _ => {}
    }
}

/// 在一段字串裡找時間戳與 UUID v4。同一段字串裡多次命中累加 count。
fn scan_string(text: &str, path: &[String], out: &mut Accumulator) {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0usize;
    while i < n {
        if out.full() {
            return;
        }
        // 先試 ISO-8601：視窗較短，字串剛好在 UUID 中間結束時比較不會漏。
        if i + ISO_LEN <= n && looks_like_iso8601(&bytes[i..i + ISO_LEN]) {
            out.add(
                VolatileKind::Timestamp,
                path,
                text[i..i + ISO_LEN].to_string(),
            );
            i += ISO_LEN;
            continue;
        }
        if i + UUID_LEN <= n && looks_like_uuid_v4(&bytes[i..i + UUID_LEN]) {
            out.add(
                VolatileKind::Uuid,
                path,
                format!("{}…", &text[i..i + UUID_SAMPLE_CHARS]),
            );
            i += UUID_LEN;
            continue;
        }
        i += 1;
    }
}

// ─── location 設界 ────────────────────────────────────────────────────

/// 限制單一 key 名的長度（以**字元**計，與 Python 的 `len(str)` 對齊）。
///
/// 副作用要知道：兩個只在第 41 字元之後才不同的 key 會被折成同一個 location，
/// 於是被 `Accumulator` 併成一筆（count 累加）。刻意的取捨。
fn cap_segment(key: &str) -> String {
    if key.chars().count() <= MAX_LOCATION_SEGMENT_CHARS {
        return key.to_string();
    }
    let head: String = key.chars().take(MAX_LOCATION_SEGMENT_CHARS).collect();
    format!("{head}…")
}

/// 把片段堆疊串成 location，並限制總長。保頭也保尾 —— 頭部是 `system` /
/// `tools[0]` 這類定位資訊，尾部是真正命中的欄位名，中段最不重要。
fn render_location(path: &[String]) -> String {
    let joined: String = path.concat();
    let len = joined.chars().count();
    if len <= MAX_LOCATION_CHARS {
        return joined;
    }
    let keep = (MAX_LOCATION_CHARS - 1) / 2;
    let head: String = joined.chars().take(keep).collect();
    let tail: String = joined.chars().skip(len - keep).collect();
    format!("{head}…{tail}")
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
    ID_FIELD_NEEDLES
        .iter()
        .any(|needle| lowered.contains(needle))
}

/// 空字串 / 空陣列 / 空物件 / null 視為「沒有值」——
/// 只是在 schema 裡『宣告』了 request_id 的 client 不該被指控。
fn is_value_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
    }
}

/// 把值渲染成**型別描述**，絕不含值本身。
///
/// 長度以**字元**計（Python 端用 `len(str)`）—— 用 byte 會讓非 ASCII 值的
/// 描述在兩邊分岔。這也是為什麼這裡不碰 `Number` 的字面值：
/// `arbitrary_precision` 只保住小數尾隨零，`1E5` 仍會被正規化成 `1e+5`。
fn describe(value: &Value) -> String {
    match value {
        Value::String(s) => format!("string[{}]", s.chars().count()),
        Value::Bool(_) => "bool".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::Array(a) => format!("array[{}]", a.len()),
        Value::Object(m) => format!("object[{}]", m.len()),
        Value::Null => "null".to_string(),
    }
}
