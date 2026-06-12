//! M7 — 可跑的 proxy 入口（2026-06-12）。
//!
//! 候選 2「真流量實測」的前置：把 Claude Code 的 base URL 指到這裡，
//! 量 cache hit rate 與總成本（北極星指標，不是 token 數）。
//!
//! 用法：
//!   cargo run --example proxy_server
//!   UPSTREAM=https://api.anthropic.com PORT=8787 cargo run --example proxy_server

use headroom_lite_rs::proxy::create_app;

#[tokio::main]
async fn main() {
    let upstream =
        std::env::var("UPSTREAM").unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);

    let app = create_app(upstream.clone(), reqwest::Client::new());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind failed — 埠被占用？換 PORT 環境變數");

    eprintln!("headroom-lite proxy: http://127.0.0.1:{port} -> {upstream}");
    axum::serve(listener, app).await.expect("server error");
}
