//! parity 工具（M23）：stdin 讀 body bytes → volatile 唯讀掃描 → stdout 每行一筆 finding，
//! 撞上限時最後多一行 `{"truncated":true}`。
//!
//! 報告刻意用 compact JSON 一行一筆，而不是自訂分隔符：location 來自客戶的
//! key 名，裡面可能有 tab / 換行 / 引號，自訂分隔符會被內容打穿。serde_json
//! 與 Python `json.dumps(ensure_ascii=False)` 的跳脫規則相同（控制字元
//! \uXXXX、非 ASCII 原樣輸出），所以兩邊的報告可以逐字節比對。

use std::io::{Read, Write};

use headroom_lite_rs::volatile::scan_request;
use serde_json::json;

fn main() {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).expect("read stdin");

    let scan = scan_request(&raw);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for finding in &scan.findings {
        let line = json!({
            "kind": finding.kind.as_str(),
            "location": finding.location,
            "sample": finding.sample,
            "count": finding.count,
        });
        // `| head` 之下 stdout 會 EPIPE —— parity 工具不該因此 panic。
        if writeln!(out, "{line}").is_err() {
            return;
        }
    }
    if scan.truncated {
        let _ = writeln!(out, "{}", json!({ "truncated": true }));
    }
}
