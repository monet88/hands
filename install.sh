#!/usr/bin/env bash
# Install grok-harness to ~/.local/bin (or $PREFIX/bin).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")" && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
CACHE="${GROK_HARNESS_CACHE:-$HOME/.cache/grok-harness}"
GROK_BUILD_URL="${GROK_BUILD_URL:-https://github.com/xai-org/grok-build.git}"
GROK_BUILD_REF="${GROK_BUILD_REF:-main}"
JOBS="${JOBS:-}"

mkdir -p "$CACHE" "$PREFIX/bin"
GROK_BUILD="$CACHE/grok-build"

if [[ -d "$GROK_BUILD/.git" ]]; then
  git -C "$GROK_BUILD" fetch --depth 1 origin "$GROK_BUILD_REF"
  git -C "$GROK_BUILD" checkout --force FETCH_HEAD
  git -C "$GROK_BUILD" clean -fdx
else
  git clone --depth 1 --branch "$GROK_BUILD_REF" "$GROK_BUILD_URL" "$GROK_BUILD" \
    || git clone --depth 1 "$GROK_BUILD_URL" "$GROK_BUILD"
fi

python3 "$REPO_ROOT/scripts/inject.py" "$REPO_ROOT" "$GROK_BUILD"

if ! command -v rustup >/dev/null 2>&1; then
  echo "rustup is required. Install: https://rustup.rs" >&2
  exit 1
fi

cd "$GROK_BUILD"
# rust-toolchain.toml in grok-build pins the compiler.
CARGO_ARGS=(build --release -p grok-harness)
if [[ -n "$JOBS" ]]; then
  CARGO_ARGS+=(-j "$JOBS")
fi
cargo "${CARGO_ARGS[@]}"

BIN="$GROK_BUILD/target/release/grok-harness"
install -m 0755 "$BIN" "$PREFIX/bin/grok-harness"

echo
echo "installed $PREFIX/bin/grok-harness"
"$PREFIX/bin/grok-harness" --version
echo
echo "Next:"
echo "  cd /your/repo && grok-harness use"
echo "  brew install openai/tools/tunnel-client   # ChatGPT Web tunnel"
echo "  see README.md for ChatGPT plugin steps"
echo
if ! command -v grok-harness >/dev/null 2>&1; then
  echo "Put $PREFIX/bin on PATH, e.g.:"
  echo "  export PATH=\"$PREFIX/bin:\$PATH\""
fi
