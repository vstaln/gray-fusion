#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
sys="$(uname -s)"
case "$sys" in
MINGW* | MSYS* | CYGWIN*) rel="x86_64-pc-windows-msvc/gray-fusion.exe" ;;
Darwin*)
    case "$(uname -m)" in
    arm64) rel="aarch64-apple-darwin/gray-fusion" ;;
    *) rel="x86_64-apple-darwin/gray-fusion" ;;
    esac ;;
*)
    case "$(uname -m)" in
    aarch64 | arm64) rel="aarch64-unknown-linux-musl/gray-fusion" ;;
    *) rel="x86_64-unknown-linux-musl/gray-fusion" ;;
    esac ;;
esac
if [ -x "$here/$rel" ]; then exec "$here/$rel" "$@"; fi
for cand in "$here/target/release/gray-fusion.exe" "$here/target/release/gray-fusion" "$here/target/debug/gray-fusion.exe" "$here/target/debug/gray-fusion"; do
    if [ -x "$cand" ]; then exec "$cand" "$@"; fi
done
echo "gray-fusion binary not found in $here" >&2
exit 1
