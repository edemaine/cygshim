use crate::bridge;
use std::env;
use std::process::ExitCode;

/// Converts native path arguments and narrowly translates path-producing output.
pub const TOOL_SCRIPT: &str = r#"
cygshim_git_args=("${cygshim_args[@]}")

# Git accepts filesystem paths both as standalone arguments and as values of
# these global options.
for ((cygshim_i = 0; cygshim_i < ${#cygshim_git_args[@]}; cygshim_i++)); do
  cygshim_arg=${cygshim_git_args[cygshim_i]}
  case $cygshim_arg in
    --git-dir=*|--work-tree=*)
      cygshim_prefix=${cygshim_arg%%=*}=
      cygshim_value=${cygshim_arg#*=}
      cygshim_git_args[cygshim_i]="$cygshim_prefix$(cygshim_to_posix_if_windows_absolute "$cygshim_value")"
      ;;
    *)
      cygshim_git_args[cygshim_i]=$(cygshim_to_posix_if_windows_absolute "$cygshim_arg")
      ;;
  esac
done

# Filter only rev-parse forms whose output is documented as one path per line.
# Applying path conversion to arbitrary Git output would corrupt diffs, file
# contents, object data, and NUL-delimited records.
cygshim_git_path_output=0
cygshim_git_rev_parse=0
cygshim_git_absolute_paths=0
for cygshim_arg in "${cygshim_git_args[@]}"; do
  if [[ $cygshim_arg == rev-parse ]]; then
    cygshim_git_rev_parse=1
  elif [[ $cygshim_git_rev_parse == 1 ]]; then
    case $cygshim_arg in
      --show-toplevel|--show-superproject-working-tree|--absolute-git-dir)
        cygshim_git_path_output=1
        ;;
      --path-format=absolute)
        cygshim_git_absolute_paths=1
        ;;
      --git-dir|--git-common-dir)
        if [[ $cygshim_git_absolute_paths == 1 ]]; then
          cygshim_git_path_output=1
        fi
        ;;
    esac
  fi
done

# Most commands can replace the shell and retain byte-for-byte stream behavior.
if [[ $cygshim_git_path_output == 0 ]]; then
  exec /usr/bin/git "${cygshim_git_args[@]}"
fi

# Preserve Git's status rather than returning the path filter's status.
set -o pipefail
/usr/bin/git "${cygshim_git_args[@]}" | while IFS= read -r cygshim_line || [[ -n $cygshim_line ]]; do
  if [[ $cygshim_line == /* ]]; then
    cygpath -m -- "$cygshim_line"
  else
    printf '%s\n' "$cygshim_line"
  fi
done
cygshim_status=${PIPESTATUS[0]}
exit "$cygshim_status"
"#;

/// Runs Cygwin Git and returns its status as the native shim's exit code.
pub fn run() -> ExitCode {
    match bridge::make_command(TOOL_SCRIPT, env::args_os().skip(1))
        .and_then(|mut command| command.status())
    {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("cygshim: {error}");
            ExitCode::FAILURE
        }
    }
}
