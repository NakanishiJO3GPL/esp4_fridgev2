#!/bin/sh
# Host-side module tests only; no firmware build, network setup, or flashing.
set -eu

cd "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

for tool in rustc node python3 cmake; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'Required tool not found: %s\n' "$tool" >&2
        exit 1
    fi
done

mkdir -p target/module-tests

# Compile for the host, independently of Cargo's ESP32-P4 target setting.
for test in camera_buffers camera_stop trigger wifi_http wifi_nvs wifi_store; do
    printf '\n=== %s ===\n' "$test"
    rustc --edition=2021 -D warnings --test "tests/$test.rs" \
        -o "target/module-tests/${test}_tests"
    "./target/module-tests/${test}_tests"
done

printf '\n=== wifi_page ===\n'
node --test tests/wifi_page.mjs

# network_defaults requires IDF_PATH or a previously generated SDK CMake cache.
printf '\n=== network_defaults ===\n'
python3 tests/network_defaults.py -v

printf '\n=== official_sdk ===\n'
python3 tests/official_sdk.py -v

printf '\nAll module test suites passed.\n'
