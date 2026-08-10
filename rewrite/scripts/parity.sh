#!/usr/bin/env bash
# parity gate — Python 與 Rust pipeline 跨語言 byte-for-byte 比對（2026-06-11，M6；M8 更新）。
#
# 對每個 fixture 跑兩邊的完整 pipeline（M8 lazy registration）：
#   stabilize_request → compress_request(store) →（有壓到才 register_ccr_tool）
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

# Python 側：完整 pipeline（與 Rust process_request 同源；uv 管的 3.13 venv，勿用系統 python）
PY_PIPELINE='
import sys
from headroom_lite.ccr import CCRStore
from headroom_lite.pipeline import process_request
raw = sys.stdin.buffer.read()
out = process_request(raw, store=CCRStore())
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

# ─── 相 2：volatile 唯讀掃描 findings 跨語言比對（2026-08-10，M23）────────
#
# 掃描不改 bytes，所以相 1 完全看不見它 —— 相 1 全過只證明「沒弄壞舊路徑」，
# 新路徑得自己有 gate（M21 補 15_pytest.json 時學到的同一件事）。
echo
cargo build --quiet --example volatile_stdin --manifest-path rust-lite/Cargo.toml
VOL_BIN="rust-lite/target/debug/examples/volatile_stdin"

PY_VOLATILE='
import json, sys
from headroom_lite.volatile import scan_request
for f in scan_request(sys.stdin.buffer.read()):
    print(json.dumps(
        {"kind": f.kind, "location": f.location, "sample": f.sample},
        ensure_ascii=False, separators=(",", ":"),
    ))
'

for fixture in "$FIXTURE_DIR"/*.json "$FIXTURE_DIR"/*.bin; do
    [ -e "$fixture" ] || continue
    name="$(basename "$fixture")"
    uv run python -c "$PY_VOLATILE" < "$fixture" > "$OUT_DIR/$name.py.vol"
    "$VOL_BIN" < "$fixture" > "$OUT_DIR/$name.rs.vol"
    n="$(wc -l < "$OUT_DIR/$name.py.vol" | tr -d ' ')"
    if cmp -s "$OUT_DIR/$name.py.vol" "$OUT_DIR/$name.rs.vol"; then
        echo "PASS  $name (volatile findings: $n)"
    else
        echo "FAIL  $name — volatile findings 兩邊不一致："
        diff "$OUT_DIR/$name.py.vol" "$OUT_DIR/$name.rs.vol" || true
        fail=1
    fi
done

# 非空守門：多數 fixture 本來就沒有 volatile 內容，findings 全空的比對會
# 安靜地印一整排 PASS 而什麼都沒驗到。17_volatile.json 就是為此存在的，
# 它的筆數寫死在這裡 —— 改動那份 fixture 必須連帶改這個數字。
EXPECTED_VOLATILE_FINDINGS=4
got="$(wc -l < "$OUT_DIR/17_volatile.json.py.vol" | tr -d ' ')"
if [ "$got" -ne "$EXPECTED_VOLATILE_FINDINGS" ]; then
    echo "FAIL  17_volatile.json — 期望 $EXPECTED_VOLATILE_FINDINGS 筆 findings，實得 $got"
    echo "      （這條守的是「比對不得空轉」，不是掃描器本身）"
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    echo "volatile parity gate: FAIL"
    exit 1
fi
echo "volatile parity gate: ALL PASS"
