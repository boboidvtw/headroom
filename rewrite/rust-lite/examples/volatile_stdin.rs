//! parity 工具（M23）：stdin 讀 body bytes → volatile 唯讀掃描 → stdout 每行一筆 finding。
//!
//! 報告刻意用 compact JSON 一行一筆，而不是自訂分隔符：sample 是客戶內容，
//! 裡面可能有 tab / 換行 / 引號，自訂分隔符會被內容打穿。serde_json 與
//! Python `json.dumps(ensure_ascii=False)` 的跳脫規則相同（控制字元 \uXXXX、
//! 非 ASCII 原樣輸出），所以兩邊的報告可以逐字節比對。

use std::io::{Read, Write};

use headroom_lite_rs::volatile::scan_request;
use serde_json::json;

fn main() {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).expect("read stdin");

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for finding in scan_request(&raw) {
        let line = json!({
            "kind": finding.kind.as_str(),
            "location": finding.location,
            "sample": finding.sample,
        });
        writeln!(out, "{line}").expect("write stdout");
    }
}
