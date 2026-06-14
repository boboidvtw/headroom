//! M6 — CCR 可逆取回 port（與 Python ccr.py 行為 / bytes 逐字對齊）。
//!
//! 差異化招牌：**壓縮永不丟資料**。
//!   - 原文搬進 content-addressed store，key = 內容的 sha256 前 16 碼
//!     —— 正是壓縮標記裡那個 hash。模型看得到 key，要原文就呼叫 ccr_retrieve。
//!   - content-addressed 的妙處：key 由內容決定，同文必同 key，
//!     store 天然去重、不需要任何協調或序號。
//!
//! register_ccr_tool 是「無條件」的純 building block：呼叫它就註冊。
//! 「何時呼叫」的決策在 pipeline 層（M8 lazy registration）—— 只在這輪
//! 真的壓到東西時才註冊，否則 tools 一個 byte 都不動。
//! 歷史教訓（2026-06-12 live traffic）：原設計每請求都註冊，害上游對
//! raw 流量的部分命中容錯失效；M8 改 lazy 治本。

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const KEY_HEX_LEN: usize = 16;

/// 內容定址 key：sha256 前 16 碼。live_zone 的標記與 store 共用
/// —— content_key 是唯一的真相來源，標記與 store 永遠對得上。
pub fn content_key(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))[..KEY_HEX_LEN].to_string()
}

/// content-addressed 原文倉庫（學習版：記憶體 HashMap）。
///
/// 正式版會是持久化 backend；介面刻意只有 put/get，換 backend 不動呼叫端。
#[derive(Default)]
pub struct CcrStore {
    items: HashMap<String, String>,
}

impl CcrStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 存入原文，回傳 key。同文同 key —— 天然去重。
    pub fn put(&mut self, text: &str) -> String {
        let key = content_key(text);
        self.items.insert(key.clone(), text.to_owned());
        key
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.items.get(key).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// 工具定義是「凍結」的：內容與 key 順序與 Python 版逐字相同，
/// 跨輪 bytes 必須逐字節一致 —— 差一個 byte，tools 前綴就炸。
/// （json! 巨集 + preserve_order：字面值順序 = 序列化順序。）
fn ccr_retrieve_tool() -> Value {
    json!({
        "name": "ccr_retrieve",
        "description": "取回先前被壓縮省略的完整原文。對話中形如 \
                        [... headroom-lite squeezed N lines | sha256:KEY ...] 的標記，\
                        代表該處原文已存放於側信道，可用 KEY 取回。",
        "input_schema": {
            "properties": {
                "key": {
                    "description": "標記中 sha256: 後的 16 碼 hex key",
                    "type": "string"
                }
            },
            "required": ["key"],
            "type": "object"
        }
    })
}

/// 把 ccr_retrieve 註冊進 body 的 tools 陣列 —— 無條件（building block）。
///
/// 註冊時機由 pipeline 決定（M8 lazy：有壓到才呼叫）。接在 tools 尾端。
/// 已存在（冪等）或壞輸入 → `Cow::Borrowed`（原始 bytes 本人）。
pub fn register_ccr_tool(raw: &[u8]) -> Cow<'_, [u8]> {
    let Ok(mut body) = serde_json::from_slice::<Value>(raw) else {
        return Cow::Borrowed(raw);
    };
    let Some(obj) = body.as_object_mut() else {
        return Cow::Borrowed(raw);
    };

    let already_registered = obj
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|t| t.get("name").and_then(Value::as_str) == Some("ccr_retrieve"))
        });
    if already_registered {
        return Cow::Borrowed(raw); // 冪等：已註冊就一個 byte 都不動
    }

    match obj.get_mut("tools").and_then(Value::as_array_mut) {
        Some(tools) => tools.push(ccr_retrieve_tool()),
        // tools 不存在或不是陣列 → 換成只含 ccr_retrieve 的新陣列
        //（與 Python 版一致；既有 key 的位置由 preserve_order 保留）
        None => {
            obj.insert("tools".into(), json!([ccr_retrieve_tool()]));
        }
    }
    match serde_json::to_vec(&body) {
        Ok(bytes) => Cow::Owned(bytes),
        Err(_) => Cow::Borrowed(raw), // 失敗模式契約：序列化失敗原樣放行
    }
}

/// 處理模型的 ccr_retrieve 呼叫：回原文，或誠實說找不到。
pub fn handle_retrieve(store: &CcrStore, key: &str) -> String {
    match store.get(key) {
        Some(original) => original.to_owned(),
        None => format!("[ccr_retrieve] 找不到 key={key} 的內容（可能已過期或 key 有誤）"),
    }
}
