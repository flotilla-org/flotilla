#!/usr/bin/env sh
# Shared by CI, candidate builds, and the crew image; requires only POSIX tools.
read_rust_pin() {
    pin=$(awk '
        /^\[toolchain\]$/ { section = 1; next }
        /^\[/ { section = 0 }
        section && /^channel = "/ { sub(/^channel = "/, ""); sub(/"$/, ""); print }
    ' "$1") || return 1
    if ! printf '%s\n' "$pin" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || [ "$(printf '%s\n' "$pin" | wc -l)" -ne 1 ]; then
        echo "expected one exact stable channel in $1 (use channel = \"X.Y.Z\" in [toolchain], without an inline comment)" >&2
        return 1
    fi
    printf '%s\n' "$pin"
}

assert_rust_pin() {
    expected=$(read_rust_pin "$1") || return 1
    actual=$(rustc --version) || return 1
    actual=$(printf '%s\n' "$actual" | awk '{print $2}')
    if [ "$actual" != "$expected" ]; then
        echo "Rust compiler mismatch: expected $expected, got $actual" >&2
        return 1
    fi
}
