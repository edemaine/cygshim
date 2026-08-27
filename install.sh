#!/usr/bin/env bash

set -euo pipefail

usage() {
  printf 'Usage: %s {-u|-g|-h|DIRECTORY}\n\n' "${0##*/}"
  printf '  -u          Install for the current user to %%LOCALAPPDATA%%\\Programs\\Cygshim\n'
  printf '  -g          Install for all users to %%ProgramFiles%%\\Cygshim\n'
  printf '  -h          Show this help\n'
  printf '  DIRECTORY   Install to a Windows or Cygwin directory\n'
}

if (( $# == 0 )); then
  usage
  exit 0
fi

if (( $# != 1 )); then
  usage >&2
  exit 2
fi

global_install=false
case $1 in
  -h | --help)
    usage
    exit 0
    ;;
  -u)
    if [[ -z ${LOCALAPPDATA:-} ]]; then
      printf 'LOCALAPPDATA is not set.\n' >&2
      exit 1
    fi
    destination="$LOCALAPPDATA\\Programs\\Cygshim"
    ;;
  -g)
    if [[ -z ${PROGRAMFILES:-} ]]; then
      printf 'PROGRAMFILES is not set.\n' >&2
      exit 1
    fi
    destination="$PROGRAMFILES\\Cygshim"
    global_install=true
    ;;
  -*)
    printf 'Unknown option: %s\n\n' "$1" >&2
    usage >&2
    exit 2
    ;;
  '')
    printf 'Installation directory cannot be empty.\n' >&2
    exit 2
    ;;
  *)
    destination=$1
    ;;
esac

# Resolve relative destinations against the caller's working directory.
install_directory=$(cygpath -u -a -- "$destination")
windows_directory=$(cygpath -w -a -- "$install_directory")

project_directory=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$project_directory"

installation_failed() {
  printf 'Could not install Cygshim to %s.\n' "$windows_directory" >&2
  if [[ $global_install == true ]]; then
    printf 'A global installation normally requires an elevated Cygwin shell.\n' >&2
  fi
  exit 1
}

printf 'Building Cygshim release executables...\n'
./build.sh

if ! mkdir -p -- "$install_directory"; then
  installation_failed
fi

executables=(git.exe latexmk.exe pdflatex.exe)
for executable in "${executables[@]}"; do
  if ! install -m 755 -- \
    "target/release/$executable" "$install_directory/$executable"; then
    installation_failed
  fi
done

printf 'Installed Cygshim to %s:\n' "$windows_directory"
for executable in "${executables[@]}"; do
  printf '  %s\\%s\n' "$windows_directory" "$executable"
done
printf '%s\n' \
  '' \
  "Cygshim did not modify PATH. Add this directory before Cygwin's bin directory."
