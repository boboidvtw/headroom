//! M8 — lazy registration：stabilize → compress →（有壓到才 register）。
//!
//! 對齊 Python 側 `headroom_lite.pipeline.process_request`。
//!
//! 歷史教訓（2026-06-12 live traffic 實測）：原設計每請求都先註冊
//! ccr_retrieve，無條件動 tools（cache 前綴最前面），害上游對 raw 流量
//! 的部分命中容錯失效。M8 把註冊決策從 building block 上移到這層：
//! 多數請求不壓縮 → tools 全程不動 → 零 cache 影響。
//!
//! Rust 版的功課仍是 Cow 生命週期接力：compress 借用 stabilized，
//! 在內部 scope 把借用結束、抽出「有壓到」的 owned bytes，
//! 才能在 None 分支把 stabilized 移出去當回傳值。
//! 契約：沒壓到 → 回 stabilized（可能 `Borrowed(raw)`，即原始 bytes 本人）；
//!       有壓到 → 註冊 ccr_retrieve（接 tools 尾端）後 `Owned` 收尾。

use std::borrow::Cow;

use crate::cache_stabilization::stabilize_request;
use crate::ccr::{register_ccr_tool, CcrStore};
use crate::live_zone::compress_request_with_store;

/// 入口：對 /v1/messages 的 body bytes 跑完整 headroom-lite pipeline。
///
/// store（可選）：給了就在壓縮前收存原文，可逆取回（M4）。
pub fn process_request<'a>(raw: &'a [u8], store: Option<&mut CcrStore>) -> Cow<'a, [u8]> {
    let stabilized = stabilize_request(raw);

    // compress 借用 stabilized；用內部 scope 結束借用、抽出「有壓到」的 owned bytes。
    // 沒壓到時 compress 回 Borrowed（identity 放行）—— stabilize 單獨動過手
    // 不算「壓縮」，不該觸發 ccr 註冊（lazy 的精髓）。
    let compressed_owned: Option<Vec<u8>> = match compress_request_with_store(&stabilized, store) {
        Cow::Borrowed(_) => None,
        Cow::Owned(bytes) => Some(bytes),
    };

    match compressed_owned {
        // 沒壓到 → 不註冊；回 stabilized（可能是原始 bytes 本人）。
        None => match stabilized {
            Cow::Borrowed(_) => Cow::Borrowed(raw),
            Cow::Owned(bytes) => Cow::Owned(bytes),
        },
        // 有壓到 → 註冊 ccr_retrieve（接已排序 client tools 的尾端）。
        Some(compressed) => Cow::Owned(register_ccr_tool(&compressed).into_owned()),
    }
}
