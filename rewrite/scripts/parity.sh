#!/usr/bin/env bash
# parity gate — Python 與 Rust pipeline 跨語言 byte-for-byte 比對（2026-06-11，M6）。
#
# 對每個 fixture 跑兩邊的完整 pipeline：
#   register_ccr_tool → stabilize_request → compress_request(store)
# 任一 fixture 兩邊輸出有一個 byte 不同 → 整個 gate FAIL（exit 1）。
#
# 用法：cd rewrite && ./scripts/parity.sh
set -euo pipefail

cd "$(dirname "$0")/.."

FIXTURE_DIR="tests/fixtures"
OUT_DIR="$(mktemp -d)"
trap 'rm -rf "$OUT_DIR"' EXIT

# Rust 側：編譯 pipeline_stdin（quiet，只在失敗時吵）
cargo build --quiet --example pipeline_stdin --manifest-path rust-lite/Cargo.toml
RUST_BIN="rust-lite/target/debug/examples/pipeline_stdin"

# Python 側：等價的一行 pipeline（uv 管的 3.13 venv，勿用系統 python）
PY_PIPELINE='
import sys
from headroom_lite.ccr import CCRStore, register_ccr_tool
from headroom_lite.cache_stabilization import stabilize_request
from headroom_lite.live_zone import compress_request
raw = sys.stdin.buffer.read()
out = compress_request(stabilize_request(register_ccr_tool(raw)), store=CCRStore())
sys.stdout.buffer.write(out)
'

fail=0
for fixture in "$FIXTURE_DIR"/*.json "$FIXTURE_DIR"/*.bin; do
    [ -e "$fixture" ] || continue
    name="$(basename "$fixture")"
    uv run python -c "$PY_PIPELINE" < "$fixture" > "$OUT_DIR/$name.py.out"
    "$RUST_BIN" < "$fixture" > "$OUT_DIR/$name.rs.out"
    if cmp -s "$OUT_DIR/$name.py.out" "$OUT_DIR/$name.rs.out"; then
        echo "PASS  $name ($(wc -c < "$fixture" | tr -d ' ') -> $(wc -c < "$OUT_DIR/$name.py.out" | tr -d ' ') bytes)"
    else
        echo "FAIL  $name — Python 與 Rust 輸出不一致："
        cmp "$OUT_DIR/$name.py.out" "$OUT_DIR/$name.rs.out" || true
        fail=1
    fi
done

if [ "$fail" -ne 0 ]; then
    echo "parity gate: FAIL"
    exit 1
fi
echo "parity gate: ALL PASS"
