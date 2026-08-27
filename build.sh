#!/usr/bin/env bash

set -euo pipefail

native_rust_bin="$(cygpath -u "$USERPROFILE")/.cargo/bin"
native_cargo="$native_rust_bin/cargo.exe"

if [[ ! -x "$native_cargo" ]]; then
  printf 'Native Windows Cargo not found at %s\n' "$native_cargo" >&2
  exit 1
fi

# Ensure Cargo selects the native rustc beside it instead of Cygwin's rustc.
export PATH="$native_rust_bin:$PATH"
# A jobserver value inherited from Cygwin Cargo is invalid for native Cargo.
unset CARGO_MAKEFLAGS

if (( $# == 0 )); then
  set -- build --release
fi

# Run the external TeX tests automatically when both wrapped tools are present.
if [[ ${1-} == test && -x /usr/bin/pdflatex && -x /usr/bin/latexmk ]]; then
  set -- test --features cygwin-tex-tests "${@:2}"
fi

exec "$native_cargo" "$@"
