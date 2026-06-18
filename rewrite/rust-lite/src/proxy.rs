//! M7 — axum HTTP 層：rust-lite 從「引擎」長成「完整 proxy」。
//!
//! 對齊 Python 版 proxy.py 的契約，加上它沒有的東西 —— 真串流：
//!   - 請求方向：POST body 過 `pipeline::process_request`，其餘 byte-faithful。
//!   - 回程方向：上游 bytes 以 stream 轉發，不在記憶體裡攢整包；
//!     SSE 回應經 `SseByteSplitter::feed_frames` 按事件邊界重切 ——
//!     下游每次收到的是「完整事件」，但 bytes 逐字節不變。
//!
//! 與 Python 版相同的設計選擇：
//!   - 工廠函式 + 注入 reqwest client（測試時指向本機 mock 上游）。
//!   - proxy 保持笨蛋：要不要壓縮是引擎（pipeline）的事，
//!     引擎說 Borrowed，proxy 就轉發原始 bytes 本人。

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CONTENT_ENCODING, CONTENT_TYPE};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::ccr::{handle_retrieve, CcrStore};
use crate::pipeline::process_request;
use crate::sse::{SseByteSplitter, SseCcrProbe};

/// resolve loop 的 hop 上限：模型若不斷追問（甚至壞掉鬼打牆），
/// proxy 必須在有限步內收手 —— 永不無限迴圈、永不抱著上游連線不放。
const MAX_RESOLVE_HOPS: usize = 8;

/// Hop-by-hop headers 不可轉發（RFC 7230 §6.1）；host / content-length
/// 由 reqwest / axum 依實際連線重算 —— 與 Python 版 _HOP_BY_HOP 同一份名單。
const HOP_BY_HOP: [&str; 10] = [
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.contains(&name)
}

struct ProxyState {
    upstream: String,
    client: reqwest::Client,
    // CCR store 是整個 proxy 唯一的可變共享狀態：壓縮前收存原文，
    // 之後模型呼叫 ccr_retrieve 才取得回（M4 的可逆性承諾）。
    store: Mutex<CcrStore>,
}

/// 建立把所有路徑轉發到 `upstream_base_url` 的 proxy app。
///
/// 測試時把 `upstream_base_url` 指向本機 mock server 即可，
/// 不需要任何 mock transport 機制 —— 走真網路棧。
pub fn create_app(upstream_base_url: String, client: reqwest::Client) -> Router {
    let state = Arc::new(ProxyState {
        upstream: upstream_base_url.trim_end_matches('/').to_string(),
        client,
        store: Mutex::new(CcrStore::new()),
    });
    Router::new()
        // M9 側信道端點：直接拿 key 換原文（debug / 工具 / 人工查證用）。
        // 顯式路由優先於 fallback —— 這條路徑不會被轉發上游。
        .route("/ccr/retrieve", post(retrieve))
        .fallback(forward)
        .with_state(state)
}

/// M9 — CCR 取回的 HTTP 入口：`POST /ccr/retrieve {"key":"..."}`。
///
/// 把 content-addressed store 變成可查端點。與 forward 共用同一個
/// `Mutex<CcrStore>`，所以「壓縮時收存的原文」這裡立刻查得到。
/// 命中 → 200 `{key, content}`；查無 → 404 `{key, error}`。
async fn retrieve(State(state): State<Arc<ProxyState>>, body: Bytes) -> Response {
    let key = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("key").and_then(Value::as_str).map(str::to_owned));
    let Some(key) = key else {
        return json_response(StatusCode::BAD_REQUEST, &json!({ "error": "missing 'key'" }));
    };

    // 鎖只罩 get、立刻放掉 —— 把 owned String 帶出鎖外再組回應。
    let found = state.store.lock().unwrap().get(&key).map(str::to_owned);
    match found {
        Some(content) => json_response(StatusCode::OK, &json!({ "key": key, "content": content })),
        None => json_response(
            StatusCode::NOT_FOUND,
            &json!({ "key": key, "error": "找不到該 key 的內容（可能已過期或 key 有誤）" }),
        ),
    }
}

fn json_response(status: StatusCode, value: &Value) -> Response {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(value).unwrap_or_default()))
        .expect("json response is always valid")
}

async fn forward(State(state): State<Arc<ProxyState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(raw) = axum::body::to_bytes(body, usize::MAX).await else {
        return error_response(StatusCode::BAD_REQUEST, "failed to read request body");
    };

    let in_len = raw.len(); // raw 之後會被 move，先記長度供觀測線用

    // 只有 POST 過引擎（/v1/messages 形狀的 body 才有 live zone 可壓；
    // 引擎對非目標 body 的契約本來就是 Borrowed 放行）。
    // 大括號刻意縮小 MutexGuard 的存活範圍 —— 不能抱著鎖跨 await。
    let body_bytes: Bytes = if parts.method == Method::POST {
        let mut store = state.store.lock().unwrap();
        match process_request(&raw, Some(&mut store)) {
            // Borrowed = 引擎沒碰過 → clone Bytes 只加引用計數，零複製
            Cow::Borrowed(_) => raw.clone(),
            Cow::Owned(v) => Bytes::from(v),
        }
    } else {
        raw
    };

    // path + query 原樣拼到上游（與 Python 版同邏輯）
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or("/", |pq| pq.as_str());
    let url = format!("{}{}", state.upstream, path_and_query);

    // 觀測線（stderr）：in == out 代表 pipeline 全程 Borrowed（原 bytes 本人）
    eprintln!(
        "{} {} in={}B out={}B{}",
        parts.method,
        path_and_query,
        in_len,
        body_bytes.len(),
        if in_len != body_bytes.len() { "  [transformed]" } else { "" }
    );

    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &parts.headers {
        if !is_hop_by_hop(name.as_str()) {
            headers.append(name.clone(), value.clone());
        }
    }

    // resolve loop 的 follow-up 要重呼同一個 url / headers / 拿初呼的 body
    // 當基底 —— 先留副本（Bytes / HeaderMap clone 都便宜）。
    let method = parts.method.clone();
    let sent_body = body_bytes.clone();
    let resolve_headers = headers.clone();
    let resolve_url = url.clone();

    let upstream_resp = match state
        .client
        .request(method.clone(), url)
        .headers(headers)
        .body(body_bytes)
        .send()
        .await
    {
        Ok(resp) => resp,
        // 上游連不上 → 誠實的 502，錯誤訊息只進回應、不 panic
        Err(err) => return error_response(StatusCode::BAD_GATEWAY, &format!("upstream error: {err}")),
    };

    // ---- 回程方向 ----
    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();

    let is_sse = resp_headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    // content-encoding 在（gzip 等）= bytes 是壓縮格式，事件邊界
    // 在密文裡找不到 —— 這種流只能原樣轉發，不重切。
    let is_encoded = resp_headers.contains_key(CONTENT_ENCODING);

    // SSE 串流：M7 路徑原封不動（byte-faithful 重切）。模型在串流回應
    // 裡呼叫 ccr_retrieve 的攔截是已知 gap —— 留待後續里程碑。
    if is_sse && !is_encoded {
        let body = Body::from_stream(rechunk_sse(upstream_resp.bytes_stream()));
        return build_response(status, &resp_headers, body);
    }

    // M10 — server-side resolve loop：只在「POST /v1/messages 且回應是 JSON」時啟動。
    // 模型若回 ccr_retrieve 工具呼叫，proxy 自己從 store 取原文、塞 tool_result
    // 重呼上游，直到模型給出真正答案 —— client 全程看不到這個注入的工具。
    let is_messages_post = method == Method::POST && path_and_query.starts_with("/v1/messages");
    let is_json = resp_headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));

    if is_messages_post && is_json {
        let first_body = match upstream_resp.bytes().await {
            Ok(b) => b,
            Err(err) => {
                return error_response(StatusCode::BAD_GATEWAY, &format!("upstream read error: {err}"))
            }
        };
        let (final_status, final_headers, final_body) = resolve_loop(
            &state,
            sent_body,
            resolve_headers,
            resolve_url,
            status,
            resp_headers,
            first_body,
        )
        .await;
        return build_response(final_status, &final_headers, Body::from(final_body));
    }

    // 其餘（GET、非 JSON 回應、非 messages 路徑）：原樣串流轉發。
    build_response(status, &resp_headers, Body::from_stream(upstream_resp.bytes_stream()))
}

/// 用上游回應的 status + 過濾後 headers 組 axum Response（hop-by-hop 不轉發；
/// content-length 交給 axum 依實際 body 重算）。
fn build_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: Body,
) -> Response {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        if !is_hop_by_hop(name.as_str()) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(body)
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "invalid upstream response"))
}

/// 從上游 JSON 回應裡找出「這輪的 ccr_retrieve 呼叫」。
///
/// 回傳 `Some(calls)` 只在「有至少一個 ccr_retrieve、且沒有混入任何
/// 其他工具呼叫」時 —— 混到 client 自己的工具就回 `None`，proxy 原樣放行
/// 不吞（那是 client 該執行的）。calls 為 (tool_use_id, key)。
fn extract_ccr_calls(resp: &Value) -> Option<Vec<(String, String)>> {
    let content = resp.get("content")?.as_array()?;
    let mut calls = Vec::new();
    let mut has_foreign_tool = false;
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        if block.get("name").and_then(Value::as_str) == Some("ccr_retrieve") {
            let id = block.get("id").and_then(Value::as_str)?.to_owned();
            let key = block
                .get("input")
                .and_then(|i| i.get("key"))
                .and_then(Value::as_str)?
                .to_owned();
            calls.push((id, key));
        } else {
            has_foreign_tool = true;
        }
    }
    if calls.is_empty() || has_foreign_tool {
        None
    } else {
        Some(calls)
    }
}

/// 組 follow-up 請求 body：在 base 的 messages 尾端接上 assistant turn
/// （原樣回填模型剛吐的 content，含那些 tool_use）與 user turn（每個呼叫
/// 一個 tool_result，content = 取回的原文），並強制 `stream:false`
/// —— follow-up 是 proxy 內部往返，要 JSON 才好解析。
fn build_followup(base: &[u8], assistant_content: &Value, tool_results: Vec<Value>) -> Option<Vec<u8>> {
    let mut body: Value = serde_json::from_slice(base).ok()?;
    let obj = body.as_object_mut()?;
    let messages = obj.get_mut("messages")?.as_array_mut()?;
    messages.push(json!({ "role": "assistant", "content": assistant_content }));
    messages.push(json!({ "role": "user", "content": tool_results }));
    obj.insert("stream".into(), Value::Bool(false));
    serde_json::to_vec(&body).ok()
}

/// M10 核心：偵測 → 取回 → 重呼，迴圈到模型不再要原文（或撞 hop 上限）。
///
/// 進來時手上是「初呼」的完整回應（status / headers / body）。若它不是
/// 一輪 ccr_retrieve（沒呼叫、或混了別的工具、或不是 JSON），原樣回傳 ——
/// 這保證了「普通回應逐字節穿透、不重序列化」（plain JSON 北極星）。
async fn resolve_loop(
    state: &ProxyState,
    base_body: Bytes,
    headers: reqwest::header::HeaderMap,
    url: String,
    first_status: reqwest::StatusCode,
    first_headers: reqwest::header::HeaderMap,
    first_body: Bytes,
) -> (reqwest::StatusCode, reqwest::header::HeaderMap, Bytes) {
    let mut status = first_status;
    let mut resp_headers = first_headers;
    let mut body = first_body;
    let mut base = base_body.to_vec();

    for _ in 0..MAX_RESOLVE_HOPS {
        let Ok(resp_val) = serde_json::from_slice::<Value>(&body) else {
            break; // 不是 JSON（理論上不會，gate 已篩）→ 原樣回
        };
        let Some(calls) = extract_ccr_calls(&resp_val) else {
            break; // 沒有「純 ccr_retrieve」這輪 → 原樣回（含 plain JSON、foreign tool）
        };

        // 取原文：鎖只罩 get，組好 tool_result 再離開鎖、才 await。
        let tool_results: Vec<Value> = {
            let store = state.store.lock().unwrap();
            calls
                .iter()
                .map(|(id, key)| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": handle_retrieve(&store, key),
                    })
                })
                .collect()
        };

        let assistant_content = resp_val.get("content").cloned().unwrap_or_else(|| json!([]));
        let Some(followup) = build_followup(&base, &assistant_content, tool_results) else {
            break; // base 不是預期形狀 → 收手，回目前手上的回應
        };

        let resp = match state
            .client
            .request(Method::POST, url.clone())
            .headers(headers.clone())
            .body(followup.clone())
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => break, // 上游 follow-up 失敗 → 回目前手上的回應，不 panic
        };

        status = resp.status();
        resp_headers = resp.headers().clone();
        body = match resp.bytes().await {
            Ok(b) => b,
            Err(_) => break,
        };
        base = followup; // 下一 hop 在這次 follow-up 之上繼續累積對話
    }

    (status, resp_headers, body)
}

/// 把上游 byte stream 重切成「每 chunk 一個完整 SSE 事件」的 stream，
/// 並在過程中**被動觀察** ccr_retrieve 呼叫（M10，observe-only）。
///
/// byte-faithful 不變量（tests/sse.rs 驗證）：
///   concat(所有輸出) == concat(所有輸入)
/// 串流結束時 `take_remaining` 沖洗殘料 —— 上游最後沒帶結尾邊界
/// 的 bytes 也一個不少地交還下游。
///
/// M10 tee：每個上游 chunk 在重切之外「另外」餵給 `SseCcrProbe`（單通道、
/// 不開 task/mpsc —— 學習版本只有一個 consumer）。偵測到模型呼叫
/// ccr_retrieve 就記一條 stderr 觀測線；**bytes 一個都不動**。忠於解答本：
/// 串流神聖、狀態機只觀察；串流內取回閉環屬於別層（見 sse::SseCcrProbe）。
fn rechunk_sse<S, E>(upstream: S) -> impl Stream<Item = Result<Bytes, E>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
{
    struct Rechunk<S> {
        inner: S,
        splitter: SseByteSplitter,
        probe: SseCcrProbe,
        queue: VecDeque<Bytes>,
        done: bool,
    }

    // Box::pin 讓 inner 變成 Unpin，unfold 的 async 閉包裡才能 .next()
    let state = Rechunk {
        inner: Box::pin(upstream),
        splitter: SseByteSplitter::new(),
        probe: SseCcrProbe::new(),
        queue: VecDeque::new(),
        done: false,
    };

    futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(frame) = st.queue.pop_front() {
                return Some((Ok(frame), st));
            }
            if st.done {
                return None;
            }
            match st.inner.next().await {
                Some(Ok(chunk)) => {
                    // 被動觀察：偵測到 ccr_retrieve 只記觀測線、不改 bytes。
                    for key in st.probe.feed(&chunk) {
                        eprintln!("  [sse] model called ccr_retrieve key={key} (observed, passthrough)");
                    }
                    for frame in st.splitter.feed_frames(&chunk) {
                        st.queue.push_back(Bytes::from(frame));
                    }
                    // frames 可能為空（事件還沒湊齊）→ 回圈繼續等下一個 chunk
                }
                // 上游串流出錯：把錯誤交給下游，axum 會中止回應
                Some(Err(err)) => return Some((Err(err), st)),
                None => {
                    st.done = true;
                    let rest = st.splitter.take_remaining();
                    if !rest.is_empty() {
                        st.queue.push_back(Bytes::from(rest));
                    }
                }
            }
        }
    })
}

fn error_response(status: StatusCode, message: &str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(message.to_string()))
        .expect("static error response is always valid")
}
