#!/bin/sh
# Обёртка zig cc для C-зависимостей (ring): убирает --target=<rust-триплет>, ставит свой.
args=""
for a in "$@"; do
    case "$a" in
        --target=*) ;;
        *) args="$args '$(printf %s "$a" | sed "s/'/'\\\\''/g")'" ;;
    esac
done
eval exec zig cc -target aarch64-linux-musl $args
