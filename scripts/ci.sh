#!/usr/bin/env bash
# scripts/ci.sh — run every linter and test in one command: rustfmt, Clippy,
# the test suite, and rustdoc. These are the same checks
# .github/workflows/ci.yml runs on every push (it calls this script), so a
# clean run here is a green pipeline there.
#
# Every check runs even after another has already failed, so one pass
# reports everything that is broken at once.
#
# The exit status is explicit: 0 when every check ran clean, 1 when any of
# them failed — never some other code, and never merely the last step's
# status leaking out.
#
# `--locked` makes Cargo fail instead of silently updating Cargo.lock, so a
# stale lockfile is caught here rather than drifting in CI.
#
# Usage:
#
#   scripts/ci.sh    # run the whole gate

set -uo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(dirname -- "$script_dir")"
cd "$repo_root" || exit 1

# Make the Rust toolchain findable even when this runs from a non-login shell:
# rustup installs into ~/.cargo/bin and exports it only from shell profile
# files, which such shells never source. Fail loudly rather than reporting
# every check as broken when the toolchain is genuinely absent.
if ! command -v cargo >/dev/null 2>&1; then
  if [[ -f "${HOME:-}/.cargo/env" ]]; then
    . "$HOME/.cargo/env"
  fi
  if ! command -v cargo >/dev/null 2>&1 && [[ -x "${HOME:-}/.cargo/bin/cargo" ]]; then
    PATH="$HOME/.cargo/bin:$PATH"
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    printf 'cargo not found on PATH; install the Rust toolchain (rustup)\n' >&2
    exit 1
  fi
fi

failed_checks=()

# step LABEL COMMAND... — print the label, stream the command's own output
# through untouched, and remember the label when the command fails.
step() {
  local label="$1"
  shift
  printf '\n==> %s\n' "$label"
  if ! "$@"; then
    failed_checks+=("$label")
    printf 'FAILED: %s\n' "$label" >&2
  fi
}

step 'rustfmt' cargo fmt --all --check
step 'Clippy' cargo clippy --locked --all-targets --all-features
step 'tests' cargo test --locked --all-targets --all-features
step 'rustdoc' env RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps --all-features

# Exit explicitly: green means 0, anything else means 1.
printf '\n'
if ((${#failed_checks[@]} == 0)); then
  printf 'All checks passed.\n'
  exit 0
fi

printf 'Failed checks:\n'
for check in "${failed_checks[@]}"; do
  printf '  - %s\n' "$check"
done
exit 1
