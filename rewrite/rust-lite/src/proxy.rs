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
use axum::Router;
use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::ccr::CcrStore;
use crate::pipeline::process_request;
use crate::sse::SseByteSplitter;

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
    Router::new().fallback(forward).with_state(state)
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

    let upstream_resp = match state
        .client
        .request(parts.method, url)
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

    let upstream_stream = upstream_resp.bytes_stream();
    let body = if is_sse && !is_encoded {
        Body::from_stream(rechunk_sse(upstream_stream))
    } else {
        Body::from_stream(upstream_stream)
    };

    let mut builder = Response::builder().status(status);
    for (name, value) in &resp_headers {
        if !is_hop_by_hop(name.as_str()) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(body)
        .unwrap_or_else(|_| error_response(StatusCode::BAD_GATEWAY, "invalid upstream response"))
}

/// 把上游 byte stream 重切成「每 chunk 一個完整 SSE 事件」的 stream。
///
/// byte-faithful 不變量（tests/sse.rs 驗證）：
///   concat(所有輸出) == concat(所有輸入)
/// 串流結束時 `take_remaining` 沖洗殘料 —— 上游最後沒帶結尾邊界
/// 的 bytes 也一個不少地交還下游。
fn rechunk_sse<S, E>(upstream: S) -> impl Stream<Item = Result<Bytes, E>>
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
{
    struct Rechunk<S> {
        inner: S,
        splitter: SseByteSplitter,
        queue: VecDeque<Bytes>,
        done: bool,
    }

    // Box::pin 讓 inner 變成 Unpin，unfold 的 async 閉包裡才能 .next()
    let state = Rechunk {
        inner: Box::pin(upstream),
        splitter: SseByteSplitter::new(),
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
