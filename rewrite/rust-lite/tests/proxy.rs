//! M7 安全網：axum proxy 的 HTTP 層。
//!
//! 對應 Python 版 test_byte_faithful_passthrough.py 的精神，但手法升級：
//! Python 用 MockTransport（process 內假網路），Rust 這裡開「真」上游
//! —— 一個綁在 127.0.0.1:0 隨機埠的 axum server，攔下實際收到的
//! method / uri / headers / body bytes。proxy 也真的跑在 TCP 上。
//! 走真網路棧，streaming 路徑才測得到。

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::Router;
use futures::StreamExt;

use headroom_lite_rs::ccr::CcrStore;
use headroom_lite_rs::pipeline::process_request;
use headroom_lite_rs::proxy::create_app;

/// 已驗證「三段全 Borrowed」的 canonical body（parity gate 534→534）。
const CANONICAL: &[u8] = include_bytes!("../../tests/fixtures/03_canonical_passthrough.json");
/// 會被 pipeline 真的動手壓縮的 messy body（parity gate 14473→2524）。
const MESSY: &[u8] = include_bytes!("../../tests/fixtures/01_messy_full.json");

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
