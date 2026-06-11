//! M6 — 全 Rust pipeline：register_ccr_tool → stabilize_request → compress。
//!
//! 對齊 Python 側的一行：
//! `compress_request(stabilize_request(register_ccr_tool(raw)), store=store)`
//!
//! Rust 版的功課是 Cow 的生命週期接力：每段的輸出借用上一段的輸出，
//! 不能直接把最後一個 Cow 原樣往外丟（它借的是中間值）。
//! 契約：三段都 `Borrowed` = 整條 pipeline 沒碰過 bytes → 還回
//! 「原始 bytes 本人」；任何一段動過手 → `Owned`（複製最終結果收尾）。

use std::borrow::Cow;

use crate::cache_stabilization::stabilize_request;
use crate::ccr::{register_ccr_tool, CcrStore};
use crate::live_zone::compress_request_with_store;

/// 入口：對 /v1/messages 的 body bytes 跑完整 headroom-lite pipeline。
///
/// store（可選）：給了就在壓縮前收存原文，可逆取回（M4）。
pub fn process_request<'a>(raw: &'a [u8], store: Option<&mut CcrStore>) -> Cow<'a, [u8]> {
    let registered = register_ccr_tool(raw);
    let stabilized = stabilize_request(&registered);
    let compressed = compress_request_with_store(&stabilized, store);

    let untouched = matches!(
        (&registered, &stabilized, &compressed),
        (Cow::Borrowed(_), Cow::Borrowed(_), Cow::Borrowed(_))
    );
    if untouched {
        Cow::Borrowed(raw)
    } else {
        Cow::Owned(compressed.into_owned())
    }
}
