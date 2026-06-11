//! M6 — cache 穩定化 port（與 Python cache_stabilization.py 行為 / bytes 逐字對齊）。
//!
//! 從「不搞砸 cache」進階到「主動幫 client 提高命中率」：
//!
//!   1. tools 正規化 —— tools 在 cache 前綴最前面，client 的 tool 順序漂移
//!      會讓整條前綴從第 0 byte 開始 miss。proxy 套「確定性正規化」
//!      （按名字排序 + schema key 遞迴排序），不管 client 怎麼漂，
//!      出口都收斂成同一份 bytes。
//!   2. cache_control 自動放置 —— 紅線：client 已放任何標記 = 客戶的
//!      明確意圖，一個都不准動；只有「全身零標記」才出手補。
//!
//! 老規矩：沒事可做回 `Cow::Borrowed`（原始 bytes 本人）、壞輸入原樣放行、
//! 整個轉換必須確定性。

use std::borrow::Cow;

use serde_json::{json, Map, Value};

/// 遞迴排序所有 object 的 key，回傳全新 Value（不可變風格）。
fn sort_keys_recursively(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), sort_keys_recursively(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys_recursively).collect()),
        other => other.clone(),
    }
}

/// tools 按名字排序；input_schema 的 key 遞迴排序。
///
/// 只動 input_schema 內部 —— tool 自身的 name/description 等
/// top-level key 順序保留 client 原樣（沒必要動的不動）。
fn normalize_tools(tools: &[Value]) -> Vec<Value> {
    let mut normalized: Vec<Value> = tools
        .iter()
        .map(|tool| match tool.as_object() {
            Some(obj) if obj.get("input_schema").is_some_and(Value::is_object) => {
                let mut new_obj = obj.clone();
                // preserve_order 的 Map：insert 既有 key 保留原位置
                new_obj.insert(
                    "input_schema".into(),
                    sort_keys_recursively(&obj["input_schema"]),
                );
                Value::Object(new_obj)
            }
            _ => tool.clone(),
        })
        .collect();
    // sort_by_key 是 stable sort —— 與 Python sorted() 的穩定性語意一致
    normalized.sort_by_key(|t| {
        t.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    });
    normalized
}

/// 走訪所有可能帶 cache_control 的 content block，檢查是否有任何標記。
fn has_any_marker(body: &Map<String, Value>) -> bool {
    let block_marked =
        |b: &Value| b.as_object().is_some_and(|o| o.contains_key("cache_control"));

    let system_marked = body
        .get("system")
        .and_then(Value::as_array)
        .is_some_and(|blocks| blocks.iter().any(block_marked));
    let messages_marked = body
        .get("messages")
        .and_then(Value::as_array)
        .is_some_and(|messages| {
            messages.iter().any(|m| {
                m.get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| blocks.iter().any(block_marked))
            })
        });
    let tools_marked = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(block_marked));

    system_marked || messages_marked || tools_marked
}

/// 把標記放在 block list 的最後一個 object block 上。回傳新 list；
/// 沒有可放的位置回傳 None。
fn mark_last_block(blocks: &[Value]) -> Option<Vec<Value>> {
    let idx = blocks.iter().rposition(Value::is_object)?;
    let mut marked = blocks.to_vec();
    marked[idx]
        .as_object_mut()
        .expect("rposition 已保證是 object")
        .insert("cache_control".into(), json!({"type": "ephemeral"}));
    Some(marked)
}

/// 零標記時自動補（學習版放 2 個，上限 4）。回傳是否有放到任何標記。
///
///   標記 1：system 最後一個 block —— 涵蓋 tools + system 整段前綴。
///   標記 2：live zone 前的最後一則訊息 —— 涵蓋整段對話歷史。
///   live zone 本身不放：它下一輪就變了，快取了也命中不到。
fn place_breakpoints(body: &mut Map<String, Value>) -> bool {
    let mut changed = false;

    // 先算好新值再寫回 —— 借用檢查器強迫讀寫分離（不可變的紀律）
    let system_marked = body
        .get("system")
        .and_then(Value::as_array)
        .and_then(|blocks| mark_last_block(blocks));
    if let Some(marked) = system_marked {
        body.insert("system".into(), Value::Array(marked));
        changed = true;
    }

    let messages_marked = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            if messages.len() < 2 {
                return None;
            }
            let frozen_idx = messages.len() - 2;
            let content = messages[frozen_idx].get("content").and_then(Value::as_array)?;
            let marked = mark_last_block(content)?;
            let mut new_messages = messages.to_vec();
            new_messages[frozen_idx]
                .as_object_mut()?
                .insert("content".into(), Value::Array(marked));
            Some(new_messages)
        });
    if let Some(new_messages) = messages_marked {
        body.insert("messages".into(), Value::Array(new_messages));
        changed = true;
    }

    changed
}

/// 入口：對 body bytes 做 cache 穩定化。
///
/// 只在「真的改到東西」時才重新序列化；否則 `Cow::Borrowed`（原始 bytes 本人）。
pub fn stabilize_request(raw: &[u8]) -> Cow<'_, [u8]> {
    let Ok(body) = serde_json::from_slice::<Value>(raw) else {
        return Cow::Borrowed(raw);
    };
    let Value::Object(mut obj) = body else {
        return Cow::Borrowed(raw);
    };

    let mut changed = false;

    let normalized_tools = match obj.get("tools").and_then(Value::as_array) {
        Some(tools) if !tools.is_empty() => {
            let normalized = normalize_tools(tools);
            // 變更偵測必須用「序列化後的 bytes」比，不能用 Value ==：
            // serde_json 的 Map 相等不在乎 key 順序，但 cache 前綴在乎
            // —— M0 的教訓（語意相等 ≠ bytes 相等）在這裡再現。
            let before = serde_json::to_vec(tools).unwrap_or_default();
            let after = serde_json::to_vec(&normalized).unwrap_or_default();
            (before != after).then_some(normalized)
        }
        _ => None,
    };
    if let Some(normalized) = normalized_tools {
        obj.insert("tools".into(), Value::Array(normalized));
        changed = true;
    }

    if !has_any_marker(&obj) && place_breakpoints(&mut obj) {
        changed = true;
    }

    if !changed {
        return Cow::Borrowed(raw);
    }
    match serde_json::to_vec(&Value::Object(obj)) {
        Ok(bytes) => Cow::Owned(bytes),
        Err(_) => Cow::Borrowed(raw), // 失敗模式契約：序列化失敗原樣放行
    }
}
