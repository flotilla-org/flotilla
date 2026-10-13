# Seed the provider-supplied $HOME with the image's read-only default Rust
# toolchain (stdlib layer `rust`). rustup keeps one RUSTUP_HOME, which defaults
# to $HOME/.rustup; linking the baked toolchain there under its distributable
# name lets a checkout pinning that release resolve offline, while other pins
# install into $HOME on demand. Idempotent and safe for concurrent launches.
# Sourced by the launch shell: use prefixed names and unset them afterwards.
flotilla_rust_root=/opt/flotilla/toolchains/rust
if [ -n "${HOME-}" ] && [ -w "$HOME" ] && [ -r "$flotilla_rust_root/default-toolchain" ]; then
    flotilla_rust_home=${RUSTUP_HOME:-$HOME/.rustup}
    flotilla_rust_toolchain=$(cat "$flotilla_rust_root/default-toolchain")
    flotilla_rust_link="$flotilla_rust_home/toolchains/$flotilla_rust_toolchain"
    if [ ! -e "$flotilla_rust_link" ]; then
        mkdir -p "$flotilla_rust_home/toolchains"
        # A concurrent launch may win the race; the link existing is success.
        ln -s "$flotilla_rust_root/rustup/toolchains/$flotilla_rust_toolchain" "$flotilla_rust_link" 2>/dev/null \
            || test -e "$flotilla_rust_link"
    fi
    if ! grep -q '^default_toolchain' "$flotilla_rust_home/settings.toml" 2>/dev/null; then
        # Offline: the toolchain is already installed, so rustup only records it.
        rustup default "$flotilla_rust_toolchain" >/dev/null 2>&1 \
            || echo "flotilla: could not set the default Rust toolchain $flotilla_rust_toolchain" >&2
    fi
    unset flotilla_rust_home flotilla_rust_toolchain flotilla_rust_link
fi
unset flotilla_rust_root
