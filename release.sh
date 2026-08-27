#!/usr/bin/env bash

set -euo pipefail

project_directory=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$project_directory"

usage() {
  cat <<'EOF'
Usage: ./release.sh [--dry-run] [--publish]

No option: build, check, and package the version specified in Cargo.toml
--dry-run: check whether the release can be published
--publish: tag and publish the release to GitHub
EOF
}

die() {
  printf '%s\n' "$1" >&2
  exit 1
}

dry_run=false
publish=false
for option in "$@"; do
  case $option in
    --dry-run) dry_run=true ;;
    --publish) publish=true ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

# --dry-run takes precedence regardless of option order
if $dry_run; then
  publish=false
fi
check_github=false
if $dry_run || $publish; then
  check_github=true
fi

manifest_version=$(sed -n '/^\[package\]$/,/^\[/ s/^version = "\([^"]*\)"$/\1/p' Cargo.toml)
[[ -n $manifest_version ]] || die "Could not read the package version from Cargo.toml"
tag="v$manifest_version"

ensure_clean_main() {
  [[ -z $(git status --porcelain) ]] || die "The working tree is not clean"

  default_branch=$(gh repo view --json defaultBranchRef --jq .defaultBranchRef.name)
  current_branch=$(git branch --show-current)
  [[ $current_branch == "$default_branch" ]] ||
    die "Releases must be made from $default_branch, not $current_branch"

  git fetch --quiet origin "$default_branch"
  head_commit=$(git rev-parse HEAD)
  remote_commit=$(git rev-parse "origin/$default_branch")
  [[ $head_commit == "$remote_commit" ]] ||
    die "HEAD does not match origin/$default_branch"
}

ensure_release_available() {
  release_result=
  if release_result=$(gh release view "$tag" 2>&1); then
    die "GitHub release $tag already exists"
  elif [[ $release_result != "release not found" ]]; then
    die "$release_result"
  fi

  remote_tag=$(git ls-remote --tags origin "refs/tags/$tag")
  [[ -z $remote_tag ]] || die "Git tag $tag already exists on origin"
}

if $check_github; then
  command -v gh >/dev/null || die "GitHub CLI (gh) is required"
  gh auth status >/dev/null
  ensure_clean_main
  ensure_release_available
fi

./check.sh
./build.sh build --release --locked

asset_directory=target/release-assets
archive_base="cygshim-$tag-windows-x86_64"
archive="$asset_directory/$archive_base.zip"
checksum="$archive.sha256"
staging_directory=$(mktemp -d "${TMPDIR:-/tmp}/cygshim-release.XXXXXX")
trap 'rm -rf -- "$staging_directory"' EXIT

mkdir -p "$asset_directory" "$staging_directory/$archive_base"
cp target/release/{git,latexmk,pdflatex}.exe README.md LICENSE \
  "$staging_directory/$archive_base/"
rm -f -- "$archive" "$checksum"

# Windows' bsdtar selects ZIP format from the archive extension.
windows_tar=$(cygpath -u "${SYSTEMROOT:-C:\\Windows}/System32/tar.exe")
[[ -x $windows_tar ]] || die "Windows tar.exe is required to create the release archive"
(
  cd "$staging_directory"
  "$windows_tar" -a -c -f "$(cygpath -w "$project_directory/$archive")" "$archive_base"
)

archive_hash=$(sha256sum "$archive" | cut -d ' ' -f 1)
printf '%s  %s\n' "$archive_hash" "${archive##*/}" >"$checksum"

if $check_github; then
  # Recheck after the potentially lengthy build and test run.
  ensure_clean_main
  ensure_release_available
fi

if ! $publish; then
  if $check_github; then
    printf 'Release dry run succeeded; created assets:\n'
  else
    printf 'Created release assets without accessing GitHub:\n'
  fi
  printf '  %s\n  %s\n' "$archive" "$checksum"
else
  gh release create "$tag" \
    "$archive#Windows x86-64 binaries" \
    "$checksum#SHA-256 checksum" \
    --target "$head_commit" \
    --title "$tag" \
    --generate-notes \
    --fail-on-no-commits
fi
