//! parity 工具：stdin 讀 body bytes → live-zone 壓縮 → stdout 寫結果。
//! 用途：與 Python 版吃同一份輸入，byte-for-byte 比對（Phase I 之魂）。

use std::io::{Read, Write};

fn main() {
    let mut raw = Vec::new();
    std::io::stdin().read_to_end(&mut raw).expect("read stdin");
    let out = headroom_lite_rs::live_zone::compress_request(&raw);
    std::io::stdout().write_all(&out).expect("write stdout");
}
