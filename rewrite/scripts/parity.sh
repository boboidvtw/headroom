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
scan = scan_request(sys.stdin.buffer.read())
for f in scan.findings:
    print(json.dumps(
        {"kind": f.kind, "location": f.location, "sample": f.sample, "count": f.count},
        ensure_ascii=False, separators=(",", ":"),
    ))
if scan.truncated:
    print(json.dumps({"truncated": True}, separators=(",", ":")))
if scan.skipped_too_large:
    print(json.dumps({"skipped": True}, separators=(",", ":")))
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

# ─── 相 3：adversarial 差分（2026-08-10，M23 review 後補）─────────────────
#
# 相 2 只證明「兩邊在這 17 個 fixture 上一致」。code review 用差分 harness
# 打出 11 類分岔，17 個 fixture 一個都碰不到 —— **「兩份實作互為守門」在被
# 打過之前只是宣稱**。這一相拿刻意構造的邊界輸入來打，而且不只比對兩邊是否
# 一致：**每個案例都有 golden**。理由是這裡有一半的正確答案就是「兩邊都空」
# （NaN / BOM / 超深巢狀），只比「兩邊一致」的話，兩邊一起壞掉會安靜通過。
echo
ADV_DIR="$FIXTURE_DIR/volatile"
# 不得出現在任何輸出裡的祕密（secret_id_field.bin 的值）—— id_field 規則
# 是唯一會撈任意客戶值的，這條守它永遠不把值印出來。
SECRET="REDACTEDSECRET"
secret_fixtures=0
nonempty_goldens=0

for fixture in "$ADV_DIR"/*.bin; do
    [ -e "$fixture" ] || continue
    name="$(basename "$fixture" .bin)"
    expected="$ADV_DIR/$name.expected"
    if [ ! -e "$expected" ]; then
        echo "FAIL  $name — 缺 golden（.expected）；adversarial fixture 一律要有預期答案"
        fail=1
        continue
    fi
    uv run python -c "$PY_VOLATILE" < "$fixture" > "$OUT_DIR/adv.$name.py" 2>"$OUT_DIR/adv.$name.pyerr" || {
        echo "FAIL  $name — Python 側拋例外（契約：壞輸入必須安靜回空）："
        cat "$OUT_DIR/adv.$name.pyerr"
        fail=1
        continue
    }
    "$VOL_BIN" < "$fixture" > "$OUT_DIR/adv.$name.rs" || {
        echo "FAIL  $name — Rust 側非零退出"
        fail=1
        continue
    }
    ok=1
    cmp -s "$OUT_DIR/adv.$name.py" "$expected" || { echo "FAIL  $name — Python 與 golden 不符："; diff "$expected" "$OUT_DIR/adv.$name.py" || true; ok=0; }
    cmp -s "$OUT_DIR/adv.$name.rs" "$expected" || { echo "FAIL  $name — Rust 與 golden 不符："; diff "$expected" "$OUT_DIR/adv.$name.rs" || true; ok=0; }
    # **先確認守門有東西可擋**：只斷言「輸出不含祕密」的話，祕密一旦從
    # fixture 輸入裡消失（改個值、換個 key 名），grep 的 needle 就永遠不可能
    # 命中，整條守門安靜空轉而全綠 —— review 二輪實測打穿過。
    # 守門要同時測「該擋的擋了」與「真的有東西可擋」。
    if grep -qF "$SECRET" "$fixture"; then
        if grep -qF "$SECRET" "$OUT_DIR/adv.$name.py" "$OUT_DIR/adv.$name.rs" 2>/dev/null; then
            echo "FAIL  $name — 客戶祕密出現在 findings 輸出裡"
            ok=0
        fi
        secret_fixtures=$((secret_fixtures + 1))
    fi
    [ -s "$expected" ] && nonempty_goldens=$((nonempty_goldens + 1))
    [ "$ok" -eq 1 ] && echo "PASS  $name" || fail=1
done

# 非空守門（同相 2 的理由）：這批有一半的 golden 本來就是空的，若 fixture
# 目錄被清空或 glob 沒配到，上面整個迴圈會一次都不跑而安靜通過。
adv_count="$(find "$ADV_DIR" -name '*.bin' | wc -l | tr -d ' ')"
EXPECTED_ADV_CASES=15
EXPECTED_NONEMPTY_GOLDENS=9
EXPECTED_SECRET_FIXTURES=1
if [ "$adv_count" -ne "$EXPECTED_ADV_CASES" ]; then
    echo "FAIL  adversarial 案例數為 $adv_count，期望 $EXPECTED_ADV_CASES（新增案例請一併改這個數字）"
    fail=1
fi
# 15 個 golden 裡有一半是空的（NaN / BOM / 超深巢狀的正確答案就是「兩邊都空」）。
# 若非空 golden 歸零，整批就退化成「空對空」而什麼都沒驗到。
if [ "$nonempty_goldens" -ne "$EXPECTED_NONEMPTY_GOLDENS" ]; then
    echo "FAIL  非空 golden 數為 $nonempty_goldens，期望 $EXPECTED_NONEMPTY_GOLDENS"
    fail=1
fi
if [ "$secret_fixtures" -ne "$EXPECTED_SECRET_FIXTURES" ]; then
    echo "FAIL  含祕密的 fixture 數為 $secret_fixtures，期望 $EXPECTED_SECRET_FIXTURES —— 祕密外洩守門沒有東西可擋"
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    echo "volatile adversarial gate: FAIL"
    exit 1
fi
echo "volatile adversarial gate: ALL PASS（$adv_count 案例）"
