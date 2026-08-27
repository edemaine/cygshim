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

# Run each external TeX test automatically when its wrapped tool is present.
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  if [[ ${args[i]} != test ]]; then
    continue
  fi

  features=()
  [[ -x /usr/bin/latexmk ]] && features+=(cygwin-latexmk-test)
  [[ -x /usr/bin/pdflatex ]] && features+=(cygwin-pdflatex-test)
  if (( ${#features[@]} > 0 )); then
    feature_list=$(IFS=,; printf '%s' "${features[*]}")
    args=("${args[@]:0:i+1}" --features "$feature_list" "${args[@]:i+1}")
  fi
  break
done

exec "$native_cargo" "${args[@]}"
