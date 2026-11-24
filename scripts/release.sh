#!/bin/sh
# Сборка dist/jarvis-plugin-arm64.zip (plugin.json + spotify-plugin) под aarch64 musl.
# Нужно: rustup с целью aarch64-unknown-linux-musl; zig в PATH (для C-зависимостей); zip.
set -eu
cd "$(dirname "$0")/.."
TARGET=aarch64-unknown-linux-musl
LIMIT=$((4 * 1024 * 1024))

if command -v zig >/dev/null 2>&1; then
    # cc-rs делит CC по пробелам, а путь к репозиторию может их содержать:
    # кладём обёртки в каталог без пробелов.
    SHIM=$(mktemp -d)
    trap 'rm -rf "$SHIM"' EXIT
    cp scripts/zcc.sh "$SHIM/zcc"
    cp scripts/zar.sh "$SHIM/zar"
    chmod 755 "$SHIM/zcc" "$SHIM/zar"
    export CC_aarch64_unknown_linux_musl="$SHIM/zcc"
    export AR_aarch64_unknown_linux_musl="$SHIM/zar"
fi
# Не вшивать в бинарник локальные пути сборщика (они попадают в сообщения о панике).
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
# CARGO_ENCODED_RUSTFLAGS разделяет флаги символом 0x1f, поэтому пробелы в путях не ломают сборку.
SEP=$(printf '\037')
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$CARGO_HOME_DIR=/cargo${SEP}--remap-path-prefix=$PWD=/src${SEP}--remap-path-prefix=$HOME=/home"
cargo build --release --target "$TARGET"

BIN="target/$TARGET/release/spotify-plugin"
# ELF: магия 7f 45 4c 46, e_machine (смещение 18, little-endian) = 0xB7 (183, AArch64).
magic=$(dd if="$BIN" bs=1 count=4 2>/dev/null | od -An -tx1 | tr -d ' \n')
machine=$(dd if="$BIN" bs=1 skip=18 count=2 2>/dev/null | od -An -tx1 | tr -d ' \n')
[ "$magic" = "7f454c46" ] || { echo "не ELF: $BIN" >&2; exit 1; }
[ "$machine" = "b700" ] || { echo "не aarch64 (e_machine=$machine)" >&2; exit 1; }
echo "ELF aarch64 OK"

rm -rf dist
mkdir -p dist/pkg
cp plugin.json dist/pkg/
cp "$BIN" dist/pkg/spotify-plugin
chmod 755 dist/pkg/spotify-plugin
(cd dist/pkg && zip -q -X ../jarvis-plugin-arm64.zip plugin.json spotify-plugin)
rm -rf dist/pkg

SIZE=$(wc -c < dist/jarvis-plugin-arm64.zip | tr -d ' ')
echo "dist/jarvis-plugin-arm64.zip: $SIZE байт"
[ "$SIZE" -le "$LIMIT" ] || { echo "архив больше 4 МБ" >&2; exit 1; }
unzip -l dist/jarvis-plugin-arm64.zip
