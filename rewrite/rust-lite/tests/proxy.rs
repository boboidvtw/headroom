//! M7 安全網：axum proxy 的 HTTP 層。
//!
//! 對應 Python 版 test_byte_faithful_passthrough.py 的精神，但手法升級：
//! Python 用 MockTransport（process 內假網路），Rust 這裡開「真」上游
//! —— 一個綁在 127.0.0.1:0 隨機埠的 axum server，攔下實際收到的
//! method / uri / headers / body bytes。proxy 也真的跑在 TCP 上。
//! 走真網路棧，streaming 路徑才測得到。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::Router;
use futures::StreamExt;
use serde_json::{json, Value};

use headroom_lite_rs::ccr::{content_key, CcrStore};
use headroom_lite_rs::pipeline::process_request;
use headroom_lite_rs::proxy::create_app;

/// 已驗證「三段全 Borrowed」的 canonical body（parity gate 534→534）。
const CANONICAL: &[u8] = include_bytes!("../../tests/fixtures/03_canonical_passthrough.json");
/// 會被 pipeline 真的動手壓縮的 messy body（parity gate 14473→2524）。
const MESSY: &[u8] = include_bytes!("../../tests/fixtures/01_messy_full.json");
/// 快取熱區塞了時間戳 / UUID v4 / correlation_id 的 body（M23 觀測用）。
const VOLATILE: &[u8] = include_bytes!("../../tests/fixtures/17_volatile.json");

/// 上游實際收到的 (method, uri, headers, body)。
type CapturedRequest = (String, String, axum::http::HeaderMap, Vec<u8>);

/// 上游實際收到的東西 —— 測試的「錄音機」。
#[derive(Clone, Default)]
struct Captured {
    inner: Arc<Mutex<Option<CapturedRequest>>>,
}

impl Captured {
    fn take(&self) -> CapturedRequest {
        self.inner.lock().unwrap().take().expect("上游沒收到請求")
    }
}

/// mock 上游：錄下請求，回固定 JSON。
fn recording_upstream(captured: Captured) -> Router {
    async fn record(State(captured): State<Captured>, req: Request) -> Response {
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        *captured.inner.lock().unwrap() = Some((
            parts.method.to_string(),
            parts.uri.to_string(),
            parts.headers,
            bytes.to_vec(),
        ));
        Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(r#"{"ok":true}"#))
            .unwrap()
    }
    Router::new().fallback(record).with_state(captured)
}

/// 把 app 跑在 127.0.0.1 隨機埠上，回 base url。
async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// 起一組「mock 上游 + 受測 proxy」，回 (proxy base url, 錄音機)。
async fn spawn_proxy_with_upstream() -> (String, Captured) {
    let captured = Captured::default();
    let upstream_url = spawn(recording_upstream(captured.clone())).await;
    let proxy_url = spawn(create_app(upstream_url, reqwest::Client::new())).await;
    (proxy_url, captured)
}

#[tokio::test]
async fn canonical_body_passes_through_byte_faithful() {
    let (proxy_url, captured) = spawn_proxy_with_upstream().await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .body(CANONICAL)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    // 最關鍵的斷言：pipeline 沒事可做時，上游收到的 bytes
    // 必須與 client 送出的「逐字節」相同 —— cache 北極星。
    let (_, _, _, body) = captured.take();
    assert_eq!(body, CANONICAL);
}

#[tokio::test]
async fn volatile_scan_does_not_touch_forwarded_bytes() {
    // M23：掃描是 observe，不是 normalize。這份 fixture **一定**掃得出東西
    // （前提先斷言，否則測到的只是「沒有掃描時 bytes 也不會變」——空的比對），
    // 而上游收到的 bytes 仍必須與 pipeline 的輸出逐字節相同。
    let scan = headroom_lite_rs::volatile::scan_request(VOLATILE);
    assert_eq!(scan.findings.len(), 4, "fixture 前提變了：{scan:?}");

    let expected = process_request(VOLATILE, Some(&mut CcrStore::new())).into_owned();
    let (proxy_url, captured) = spawn_proxy_with_upstream().await;

    reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .header("content-type", "application/json")
        .body(VOLATILE)
        .send()
        .await
        .unwrap();

    let (_, _, _, body) = captured.take();
    assert_eq!(body, expected, "volatile 掃描不得影響轉發的 bytes");
}

#[tokio::test]
async fn messy_post_body_runs_through_pipeline() {
    let (proxy_url, captured) = spawn_proxy_with_upstream().await;

    // 期望值直接問引擎本人 —— proxy 只是接線，不該有自己的意見
    let expected = process_request(MESSY, Some(&mut CcrStore::new())).into_owned();
    assert_ne!(expected.as_slice(), MESSY); // 前提：這個 fixture 真的會被壓

    reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .body(MESSY)
        .send()
        .await
        .unwrap();

    let (_, _, _, body) = captured.take();
    assert_eq!(body, expected);
}

#[tokio::test]
async fn get_is_forwarded_with_path_and_query_untouched() {
    let (proxy_url, captured) = spawn_proxy_with_upstream().await;

    reqwest::Client::new()
        .get(format!("{proxy_url}/v1/models?limit=5&after=abc"))
        .send()
        .await
        .unwrap();

    let (method, uri, _, body) = captured.take();
    assert_eq!(method, "GET");
    assert_eq!(uri, "/v1/models?limit=5&after=abc");
    assert!(body.is_empty()); // GET 不過 pipeline、也沒 body
}

#[tokio::test]
async fn auth_headers_forwarded_host_rewritten() {
    let (proxy_url, captured) = spawn_proxy_with_upstream().await;

    reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .header("x-api-key", "sk-test-123")
        .header("anthropic-version", "2023-06-01")
        .body(CANONICAL)
        .send()
        .await
        .unwrap();

    let (_, _, headers, _) = captured.take();
    // 業務 headers 原樣到達上游
    assert_eq!(headers.get("x-api-key").unwrap(), "sk-test-123");
    assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    // host 屬 hop-by-hop 範疇：必須是「上游連線」的 host，
    // 不是 client 打 proxy 用的那個 —— 硬轉發上游會 400。
    let host = headers.get("host").unwrap().to_str().unwrap();
    assert!(host.starts_with("127.0.0.1"));
}

#[tokio::test]
async fn upstream_status_headers_body_come_back_unchanged() {
    // 這次上游不錄音，改回「有個性」的回應，盯回程方向
    let upstream = Router::new().fallback(|| async {
        Response::builder()
            .status(418)
            .header("x-upstream-flavor", "teapot")
            .body(Body::from("short and stout"))
            .unwrap()
    });
    let upstream_url = spawn(upstream).await;
    let proxy_url = spawn(create_app(upstream_url, reqwest::Client::new())).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .body(CANONICAL)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 418);
    assert_eq!(resp.headers().get("x-upstream-flavor").unwrap(), "teapot");
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"short and stout");
}

#[tokio::test]
async fn sse_stream_is_byte_faithful_end_to_end() {
    // 上游回 SSE 串流，chunk 邊界刻意切在「事件中間」與「emoji 中間」
    // —— proxy 的回程重切（SseByteSplitter::feed_frames）不准弄丟
    // 或改動任何 byte，包括最後一段沒有結尾邊界的殘料。
    let full: Vec<u8> = [
        "event: content_block_delta\ndata: {\"text\":\"前🔥後\"}\n\n",
        "event: ping\r\n\r\n",
        "event: message_stop\ndata: {}\n", // 結尾故意不完整（只有一個 \n）
    ]
    .concat()
    .into_bytes();
    let chunks: Vec<Vec<u8>> = full.chunks(7).map(<[u8]>::to_vec).collect();

    let upstream = Router::new().fallback(move || {
        let chunks = chunks.clone();
        async move {
            let stream =
                futures::stream::iter(chunks.into_iter().map(Ok::<_, std::convert::Infallible>));
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    });
    let upstream_url = spawn(upstream).await;
    let proxy_url = spawn(create_app(upstream_url, reqwest::Client::new())).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .body(CANONICAL)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    // 用串流方式收 —— 走的就是 client 真實的讀法
    let mut received = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        received.extend(chunk.unwrap());
    }
    assert_eq!(received, full);
}

// ───────────────────────────── M9 / M10 共用測試工具 ─────────────────────────

/// 可編腳本的 mock 上游：錄下每個請求 body，依序吐出預設回應。
/// 佇列空了就回一個「正常結束」的回應（不含 tool_use）—— 模擬模型答完。
#[derive(Clone)]
struct Scripted {
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    responses: Arc<Mutex<VecDeque<String>>>,
}

impl Scripted {
    fn new(responses: Vec<&str>) -> Self {
        Self {
            bodies: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses.into_iter().map(String::from).collect())),
        }
    }

    /// 上游收到的所有請求 body（依到達順序）。
    fn bodies(&self) -> Vec<Vec<u8>> {
        self.bodies.lock().unwrap().clone()
    }
}

fn scripted_upstream(s: Scripted) -> Router {
    async fn handle(State(s): State<Scripted>, req: Request) -> Response {
        let (_, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        s.bodies.lock().unwrap().push(bytes.to_vec());
        let next = s.responses.lock().unwrap().pop_front().unwrap_or_else(|| {
            r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}"#.to_string()
        });
        Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(next))
            .unwrap()
    }
    Router::new().fallback(handle).with_state(s)
}

async fn spawn_proxy_with_scripted(s: Scripted) -> String {
    let upstream_url = spawn(scripted_upstream(s)).await;
    spawn(create_app(upstream_url, reqwest::Client::new())).await
}

/// 造一個「最後一則 user turn 帶長 tool_result」的 /v1/messages body。
/// text 夠長（> 2048 bytes、> 30 行）時 pipeline 會壓縮並把原文收進 store。
fn compressible_body(text: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "claude-x",
        "messages": [{
            "role": "user",
            "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": text }]
        }]
    }))
    .unwrap()
}

/// 50 行、夠胖、確定會觸發壓縮的文字。
fn fat_text(tag: &str) -> String {
    (0..50)
        .map(|i| format!("{tag} line {i}: lorem ipsum dolor sit amet consectetur adipiscing elit"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ───────────────────────────── M9：/ccr/retrieve 端點 ─────────────────────────

#[tokio::test]
async fn ccr_retrieve_endpoint_returns_stored_original() {
    let proxy_url = spawn_proxy_with_scripted(Scripted::new(vec![])).await;
    let client = reqwest::Client::new();
    let text = fat_text("alpha");

    // 先 flow 一遍可壓縮 body → pipeline 把原文收進 proxy 的 store
    client
        .post(format!("{proxy_url}/v1/messages"))
        .body(compressible_body(&text))
        .send()
        .await
        .unwrap();

    // 用標記裡那個 key 呼叫側信道端點，要回原文本人
    let key = content_key(&text);
    let resp = client
        .post(format!("{proxy_url}/ccr/retrieve"))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&json!({ "key": key })).unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let v: Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(v["key"], key);
    assert_eq!(v["content"], text);
}

#[tokio::test]
async fn ccr_retrieve_endpoint_404_for_unknown_key() {
    let proxy_url = spawn_proxy_with_scripted(Scripted::new(vec![])).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/ccr/retrieve"))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&json!({ "key": "deadbeefdeadbeef" })).unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let v: Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(v["key"], "deadbeefdeadbeef");
    assert!(v.get("error").is_some());
}

// ───────────────────────── M10：回程 server-side resolve loop ─────────────────

#[tokio::test]
async fn resolve_loop_handles_ccr_retrieve_server_side() {
    let text = fat_text("secret");
    let key = content_key(&text);

    // 上游劇本：call#1 模型呼叫 ccr_retrieve(key)；call#2（proxy 內部 follow-up）回最終答案
    let tool_use_resp = serde_json::to_string(&json!({
        "id": "msg_1", "type": "message", "role": "assistant",
        "stop_reason": "tool_use",
        "content": [{ "type": "tool_use", "id": "toolu_1", "name": "ccr_retrieve", "input": { "key": key } }]
    }))
    .unwrap();
    let final_resp = r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"FINAL ANSWER"}]}"#;
    let s = Scripted::new(vec![tool_use_resp.as_str(), final_resp]);
    let proxy_url = spawn_proxy_with_scripted(s.clone()).await;

    // 單一 client 請求：它自己的 pipeline 先把原文收進 store（key=K），
    // 上游回 ccr_retrieve(K) → proxy 自行取回、重呼上游、串回最終答案。
    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .body(compressible_body(&text))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let v: Value = serde_json::from_slice(&resp.bytes().await.unwrap()).unwrap();
    // client 看到的是最終答案，完全沒看到 ccr_retrieve 這個注入的工具
    assert_eq!(v["content"][0]["text"], "FINAL ANSWER");

    // proxy 的 follow-up（第 2 個上游請求）必須帶 resolved 原文當 tool_result
    let bodies = s.bodies();
    assert_eq!(bodies.len(), 2, "應有 1 次初呼 + 1 次 follow-up");
    let followup: Value = serde_json::from_slice(&bodies[1]).unwrap();
    let msgs = followup["messages"].as_array().unwrap();
    let last = msgs.last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"][0]["type"], "tool_result");
    assert_eq!(last["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(last["content"][0]["content"], text);
}

#[tokio::test]
async fn resolve_loop_passes_through_foreign_tool_use() {
    // 非 ccr_retrieve 的工具呼叫是 client 自己的工具 —— proxy 絕不能吞，原樣放行
    let foreign =
        r#"{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t","name":"get_weather","input":{}}]}"#;
    let s = Scripted::new(vec![foreign]);
    let proxy_url = spawn_proxy_with_scripted(s.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .body(compressible_body("x"))
        .send()
        .await
        .unwrap();

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), foreign.as_bytes());
    assert_eq!(s.bodies().len(), 1, "不該有 follow-up");
}

#[tokio::test]
async fn plain_json_response_passes_through_byte_faithful() {
    // 沒有 tool_use 的普通 JSON 回應：proxy 不重序列化，逐字節原樣交還
    //（1.50 不准被壓成 1.5 —— 與請求方向同一條北極星）
    let plain = r#"{"hello":"world","n":1.50}"#;
    let s = Scripted::new(vec![plain]);
    let proxy_url = spawn_proxy_with_scripted(s).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .body(compressible_body("x"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.bytes().await.unwrap().as_ref(), plain.as_bytes());
}

#[tokio::test]
async fn resolve_loop_stops_at_hop_cap() {
    // 上游每輪都回同一個 ccr_retrieve → 永不終止；proxy 必須在上限停手，不無限迴圈
    let text = fat_text("loopy");
    let key = content_key(&text);
    let tu = serde_json::to_string(&json!({
        "stop_reason": "tool_use",
        "content": [{ "type": "tool_use", "id": "t", "name": "ccr_retrieve", "input": { "key": key } }]
    }))
    .unwrap();
    let responses: Vec<&str> = vec![tu.as_str(); 50];
    let s = Scripted::new(responses);
    let proxy_url = spawn_proxy_with_scripted(s.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .body(compressible_body(&text))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200); // 沒掛、沒無限迴圈
    let n = s.bodies().len();
    assert!(n >= 2, "至少 loop 過一次，實得 {n}");
    assert!(n <= 1 + 8, "hops 必須有上限，實得 {n}");
}

#[tokio::test]
async fn sse_with_ccr_retrieve_stays_byte_faithful() {
    // M10 observe-only 不變量：就算串流裡模型呼叫了 ccr_retrieve，
    // proxy 也只「觀察」—— client 收到的 bytes 必須逐字節原封不動。
    let full: Vec<u8> = [
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"ccr_retrieve\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"key\\\":\\\"abc123\\\"}\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]
    .concat()
    .into_bytes();
    // chunk 邊界切在事件中間 —— 順便驗證重切器在有 probe 的情況下仍不破 bytes
    let chunks: Vec<Vec<u8>> = full.chunks(9).map(<[u8]>::to_vec).collect();

    let upstream = Router::new().fallback(move || {
        let chunks = chunks.clone();
        async move {
            let stream =
                futures::stream::iter(chunks.into_iter().map(Ok::<_, std::convert::Infallible>));
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    });
    let upstream_url = spawn(upstream).await;
    let proxy_url = spawn(create_app(upstream_url, reqwest::Client::new())).await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .body(CANONICAL)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    let mut received = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        received.extend(chunk.unwrap());
    }
    assert_eq!(received, full);
}

#[tokio::test]
async fn unreachable_upstream_returns_502() {
    // 指向沒人聽的埠 —— proxy 要誠實回 502，不是 panic 或掛著
    let proxy_url = spawn(create_app(
        "http://127.0.0.1:1".to_string(),
        reqwest::Client::new(),
    ))
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{proxy_url}/v1/messages"))
        .body(CANONICAL)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
}
