//! parity 工具：stdin 讀 body bytes → 完整 pipeline（register → stabilize → compress）→ stdout。
//! 用途：與 Python 版吃同一份輸入，byte-for-byte 比對（Phase I 之魂，M6 版）。

use std::io::{Read, Write};

use headroom_lite_rs::ccr::CcrStore;

fn main() {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).expect("read stdin");
    let mut store = CcrStore::new();
    let out = headroom_lite_rs::pipeline::process_request(&raw, Some(&mut store));
    std::io::stdout().write_all(&out).expect("write stdout");
}
