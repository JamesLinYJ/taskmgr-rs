#!/usr/bin/env bash
# +-------------------------------------------------------------------------
#
#   taskmgr-rs - GNU 诊断符号构建
#
#   文件:       scripts/build-diagnostics.sh
#
#   日期:       2026年07月27日
#   环境:       Fedora Linux 45 x86_64；Linux 内核 7.2.0-0.rc4.260725g0ce37745d4bf.39.fc45.x86_64；Rust 1.97.1；MinGW GCC 16.1.1；Wine 11.14 (Staging)
#   作者:       OpenAI Codex
# --------------------------------------------------------------------------

set -euo pipefail

target="${1:-x86_64-pc-windows-gnu}"
case "$target" in
    x86_64-pc-windows-gnu)
        objcopy="${OBJCOPY:-x86_64-w64-mingw32-objcopy}"
        ;;
    i686-pc-windows-gnu)
        objcopy="${OBJCOPY:-i686-w64-mingw32-objcopy}"
        ;;
    *)
        echo "unsupported GNU diagnostic target: $target" >&2
        exit 2
        ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile_dir="$repo_root/target/$target/diagnostics"
executable="$profile_dir/taskmgr.exe"
symbols="$profile_dir/taskmgr.exe.debug"
hash_file="$profile_dir/taskmgr.exe.sha256"
cargo_command="${CARGO:-cargo}"

cd "$repo_root"
"$cargo_command" build --profile diagnostics --target "$target"
"$objcopy" --only-keep-debug "$executable" "$symbols"
"$objcopy" --strip-debug "$executable"
(
    cd "$profile_dir"
    "$objcopy" --add-gnu-debuglink="$(basename "$symbols")" "$(basename "$executable")"
    sha256sum "$(basename "$executable")" > "$(basename "$hash_file")"
)

echo "Executable: $executable"
echo "Symbols:    $symbols"
echo "SHA-256:    $hash_file"
