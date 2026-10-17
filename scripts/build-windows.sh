#!/bin/bash
# Cross-compiles the Windows build from macOS or Linux and packs it into a zip.
# Needs the mingw-w64 toolchain (macOS: brew install mingw-w64; Debian/Ubuntu:
# apt install mingw-w64) and the Rust target (rustup target add x86_64-pc-windows-gnu).
set -euo pipefail
cd "$(dirname "$0")/.."
target=x86_64-pc-windows-gnu
cargo build --release --target "$target"
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
dist="target/dist/DBeast-$version-windows-x86_64"
rm -rf "$dist" "$dist.zip"
mkdir -p "$dist"
cp "target/$target/release/dbeast.exe" "$dist/DBeast.exe"
x86_64-w64-mingw32-strip "$dist/DBeast.exe"
cp LICENSE NOTICE README.md "$dist/"
(cd target/dist && zip -qr "$(basename "$dist").zip" "$(basename "$dist")")
echo "Built $dist.zip"
