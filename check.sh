#!/usr/bin/env bash

set -euo pipefail

project_directory=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$project_directory"

./build.sh fmt -- --check
./build.sh clippy --all-targets --all-features -- -D warnings
./build.sh test --all-targets
