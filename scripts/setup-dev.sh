#!/usr/bin/env bash
# setup-dev.sh — install pinned toolchain + system deps for heron.
#
# Idempotent: prints what's already installed and skips it. Run
# from a fresh macOS install or after pulling a toolchain bump.

set -euo pipefail

# Bash 3.2 doesn't have associative arrays; this script intentionally
# uses only POSIX-shell-compatible constructs so it runs on the macOS
# system bash without a Homebrew bash detour.

note() { printf "→ %s\n" "$*"; }
ok()   { printf "✓ %s\n" "$*"; }
miss() { printf "✘ %s\n" "$*" >&2; }

ensure_command() {
    local name="$1"
    local install_hint="$2"
    if command -v "$name" >/dev/null 2>&1; then
        ok "$name found at $(command -v "$name")"
    else
        miss "$name missing — $install_hint"
        return 1
    fi
}

OS="$(uname -s)"
if [ "$OS" != "Darwin" ]; then
    miss "heron v1 is macOS-only; setup-dev on $OS may miss steps"
fi

###
### Homebrew (macOS only)
###

if [ "$OS" = "Darwin" ]; then
    note "Checking Homebrew"
    if ! ensure_command brew "install from https://brew.sh"; then
        miss "heron's macOS deps install via brew; aborting until brew is available"
        exit 1
    fi
fi

###
### Rust toolchain (pinned via rust-toolchain.toml)
###

note "Checking rustup"
if ! command -v rustup >/dev/null 2>&1; then
    miss "rustup missing — install via 'brew install rustup-init && rustup-init' or https://rustup.rs"
    exit 1
fi
ok "rustup at $(command -v rustup)"

note "Activating pinned Rust toolchain (rust-toolchain.toml resolves it)"
# Force rustup to fetch the pin if it isn't already.
rustup show active-toolchain >/dev/null

note "Checking pinned components"
rustup component add rustfmt clippy

###
### cargo-deny (workspace policy)
###

if cargo deny --version >/dev/null 2>&1; then
    ok "cargo-deny at $(command -v cargo-deny)"
else
    note "Installing cargo-deny"
    cargo install cargo-deny --locked
fi

###
### Bun (Tauri shell frontend)
###

note "Checking Bun"
if ! command -v bun >/dev/null 2>&1; then
    if [ "$OS" = "Darwin" ]; then
        note "Installing Bun via Homebrew"
        brew install oven-sh/bun/bun
    else
        miss "bun missing — install per https://bun.sh"
        exit 1
    fi
fi
ok "bun at $(command -v bun)"

###
### ffmpeg / ffprobe (m4a archive encode + verify, §11.3 + §12.3)
###

if [ "$OS" = "Darwin" ]; then
    if ! brew list ffmpeg >/dev/null 2>&1; then
        note "Installing ffmpeg via Homebrew"
        brew install ffmpeg
    fi
fi
ensure_command ffmpeg "brew install ffmpeg" || exit 1
ensure_command ffprobe "brew install ffmpeg" || exit 1

###
### Swift (already shipped with Xcode on macOS)
###

if [ "$OS" = "Darwin" ]; then
    note "Checking Swift toolchain"
    if ! xcrun -f swiftc >/dev/null 2>&1; then
        miss "Swift toolchain not resolvable via xcrun — install Xcode Command Line Tools: 'xcode-select --install'"
        exit 1
    fi
    ok "swiftc at $(xcrun -f swiftc)"
fi

###
### Pre-flight smoke
###

note "Running 'cargo build --workspace --all-targets' as a smoke test (no-op on warm cache)"
# --all-targets pulls dev-dependencies and compiles tests + examples
# too, which is a more honest "the dev env actually works" check
# than just compiling the lib targets.
cargo build --workspace --all-targets --quiet

ok "Setup complete. Try: cargo run --bin heron -- status"
