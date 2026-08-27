use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Win32 process flag that suppresses a console window for background calls.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Reserved environment namespace removed before transport variables are set.
const ENV_PREFIX: &str = "CYGSHIM_";
/// The only command text parsed while Cygwin's inherited glob mode is active.
const BOOTSTRAP: &str = "eval \"$CYGSHIM_SCRIPT\"";

/// Trusted Bash shared by every shim to reconstruct argv and translate inputs.
///
/// Argument values arrive through the environment and are expanded only as
/// quoted array elements; they are never parsed as part of this script.
const PRELUDE: &str = r#"
cygshim_args=()
cygshim_i=0
while [[ -v CYGSHIM_ARG_$cygshim_i ]]; do
  cygshim_name=CYGSHIM_ARG_$cygshim_i
  cygshim_args+=("${!cygshim_name}")
  unset "$cygshim_name"
  ((cygshim_i++))
done

if [[ -v CYGSHIM_ORIGINAL_CYGWIN ]]; then
  export CYGWIN=$CYGSHIM_ORIGINAL_CYGWIN
else
  unset CYGWIN
fi

unset CYGSHIM_ORIGINAL_CYGWIN CYGSHIM_SCRIPT cygshim_i cygshim_name

cygshim_is_windows_absolute_path() {
  local path=$1

  if [[ ${path:0:8} == '\\?\UNC\' ]]; then
    return 0
  fi
  if [[ ${path:0:4} == '\\?\' ]]; then
    path=${path:4}
  fi

  [[ ${#path} -ge 3 && ${path:1:1} == : && ( ${path:2:1} == / || ${path:2:1} == '\' ) ]] ||
    [[ ${path:0:2} == '\\' || ${path:0:2} == // ]]
}

cygshim_to_posix_path() {
  local path=$1

  if [[ ${path:0:8} == '\\?\UNC\' ]]; then
    path="\\\\${path:8}"
  elif [[ ${path:0:4} == '\\?\' ]]; then
    path=${path:4}
  fi

  cygpath -u -- "$path"
}

cygshim_to_posix_if_windows_absolute() {
  if cygshim_is_windows_absolute_path "$1"; then
    cygshim_to_posix_path "$1"
  else
    printf '%s\n' "$1"
  fi
}
"#;

/// Builds, but does not start, the Cygwin Bash process for a trusted tool script.
///
/// The returned command carries arguments through numbered environment
/// variables and temporarily removes `CYGWIN=noglob` only for the fixed
/// native-to-Cygwin bootstrap.
pub fn make_command(
    tool_script: &str,
    args: impl IntoIterator<Item = OsString>,
) -> io::Result<Command> {
    let bash = find_bash()?;
    let args = args.into_iter().collect::<Vec<_>>();
    let original_cygwin = env::var_os("CYGWIN");
    let mut command = Command::new(bash);

    command.args(["-c", BOOTSTRAP]);

    for (name, _) in env::vars_os() {
        if starts_with_ignore_ascii_case(&name, ENV_PREFIX) {
            command.env_remove(name);
        }
    }

    // CYGWIN=noglob avoids Cygwin's wildcard expansion but has different quote
    // parsing. The fixed bootstrap is safe under normal glob parsing; the real
    // arguments never cross that parser. Restore the caller's setting before
    // the wrapped Cygwin program starts.
    match &original_cygwin {
        Some(value) => {
            command.env("CYGWIN", without_noglob(value));
            command.env("CYGSHIM_ORIGINAL_CYGWIN", value);
        }
        None => {
            command.env_remove("CYGWIN");
        }
    }

    command.env("CYGSHIM_SCRIPT", format!("{PRELUDE}\n{tool_script}"));
    for (index, arg) in args.iter().enumerate() {
        command.env(format!("CYGSHIM_ARG_{index}"), arg);
    }

    // Avoid console-window flashes when GUI applications run background commands.
    if !io::stdin().is_terminal() && !io::stdout().is_terminal() && !io::stderr().is_terminal() {
        command.creation_flags(CREATE_NO_WINDOW);
    }

    Ok(command)
}

/// Finds Bash in a Cygwin installation, rejecting Bash from other environments.
fn find_bash() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("CYGSHIM_BASH") {
        let path = PathBuf::from(path);
        return validate_bash(path, "CYGSHIM_BASH");
    }

    let path_directories = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();

    // Prefer the more Cygwin-specific `cygpath` executable and ask each
    // candidate for its mounted root.
    let cygpath_candidates = path_directories
        .iter()
        .map(|directory| directory.join("cygpath.exe"))
        .filter(|path| path.is_file())
        .filter_map(|path| cygwin_root_from_cygpath(&path))
        .map(|root| root.join("bin").join("bash.exe"));

    // Retain direct Bash discovery for PATH layouts without a usable cygpath.
    let path_candidates = path_directories
        .iter()
        .map(|directory| directory.join("bash.exe"));
    let conventional_candidates =
        [r"C:\cygwin64\bin\bash.exe", r"C:\cygwin\bin\bash.exe"].map(PathBuf::from);
    let candidates = cygpath_candidates
        .chain(path_candidates)
        .chain(conventional_candidates);

    // The lazy chain stops at the first candidate with Cygwin's DLL beside it,
    // rejecting similarly named tools from Git for Windows and MSYS2.
    if let Some(path) = candidates.into_iter().find(|path| is_cygwin_bash(path)) {
        return Ok(path);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not find Cygwin bash.exe; put Cygwin's bin directory on PATH or set CYGSHIM_BASH",
    ))
}

/// Asks a `cygpath.exe` candidate for its installation's mounted root.
fn cygwin_root_from_cygpath(cygpath: &Path) -> Option<PathBuf> {
    let mut command = Command::new(cygpath);
    // Force a known encoding before converting cygpath's output to a PathBuf.
    command
        .args(["-C", "UTF8", "-w", "/"])
        .creation_flags(CREATE_NO_WINDOW);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim_end_matches(['\r', '\n']);
    (!root.is_empty()).then(|| PathBuf::from(root))
}

fn validate_bash(path: PathBuf, variable: &str) -> io::Result<PathBuf> {
    if is_cygwin_bash(&path) {
        Ok(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{variable} does not identify Cygwin bash.exe: {}",
                path.display()
            ),
        ))
    }
}

fn is_cygwin_bash(path: &Path) -> bool {
    path.is_file()
        && path
            .parent()
            .is_some_and(|directory| directory.join("cygwin1.dll").is_file())
}

/// Removes only the `noglob` token while retaining the caller's other options.
fn without_noglob(value: &OsStr) -> OsString {
    let filtered = value
        .to_string_lossy()
        .split_ascii_whitespace()
        .filter(|option| !option.eq_ignore_ascii_case("noglob"))
        .collect::<Vec<_>>()
        .join(" ");
    OsString::from(filtered)
}

fn starts_with_ignore_ascii_case(value: &OsStr, prefix: &str) -> bool {
    value
        .to_string_lossy()
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_noglob_from_cygwin_options() {
        assert_eq!(
            without_noglob(OsStr::new("winsymlinks:native noglob")),
            "winsymlinks:native"
        );
        assert_eq!(without_noglob(OsStr::new("noglob")), "");
        assert_eq!(
            without_noglob(OsStr::new("glob:ignorecase")),
            "glob:ignorecase"
        );
    }

    #[test]
    fn transports_arguments_losslessly() {
        let arguments = [
            OsString::from(""),
            OsString::from("plain"),
            OsString::from("with spaces"),
            OsString::from("[brackets]"),
            OsString::from("{braces}"),
            OsString::from("*.glob"),
            OsString::from("literal \"quote\""),
            OsString::from("backslash\\\"quote"),
            OsString::from("line one\nline two"),
        ];
        let script = r#"printf '%s\0' "${cygshim_args[@]}""#;
        let output = make_command(script, arguments.clone())
            .unwrap()
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = arguments
            .iter()
            .flat_map(|argument| {
                let mut bytes = argument.to_string_lossy().as_bytes().to_vec();
                bytes.push(0);
                bytes
            })
            .collect::<Vec<_>>();
        assert_eq!(output.stdout, expected);
    }

    #[test]
    fn restores_the_callers_cygwin_variable() {
        let script = r#"
if [[ -v CYGWIN ]]; then
  printf 'set\0%s' "$CYGWIN"
else
  printf 'unset\0'
fi
"#;
        let output = make_command(script, []).unwrap().output().unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = match env::var_os("CYGWIN") {
            Some(value) => {
                let mut expected = b"set\0".to_vec();
                expected.extend_from_slice(value.to_string_lossy().as_bytes());
                expected
            }
            None => b"unset\0".to_vec(),
        };
        assert_eq!(output.stdout, expected);
    }

    #[test]
    fn does_not_expose_transport_variables_to_the_tool() {
        let script = r#"printf '%s\0' "${!CYGSHIM_@}""#;
        let output = make_command(script, []).unwrap().output().unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"\0");
    }

    #[test]
    fn converts_extended_windows_drive_paths() {
        let path = r"\\?\C:\Users\edemaine";
        let script = r#"cygshim_to_posix_if_windows_absolute "${cygshim_args[0]}""#;
        let output = make_command(script, [OsString::from(path)])
            .unwrap()
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"/cygdrive/c/Users/edemaine\n");
    }

    #[test]
    fn converts_extended_windows_unc_paths() {
        let path = r"\\?\UNC\server\share\folder";
        let script = r#"cygshim_to_posix_if_windows_absolute "${cygshim_args[0]}""#;
        let output = make_command(script, [OsString::from(path)])
            .unwrap()
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"//server/share/folder\n");
    }
}
