#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("cygshim-{name}-{}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn git_preserves_arguments_with_inherited_noglob() {
    let git = env!("CARGO_BIN_EXE_git");
    let quoted = Command::new(git)
        .args([
            "rev-parse",
            "--sq-quote",
            "",
            "literal \"quote\"",
            "[brackets]",
            "{braces}",
            "*.glob",
            "literal $(printf substituted) value",
            "literal `printf substituted` value",
            "O'Brien",
            r"literal\value",
        ])
        .env("CYGWIN", "winsymlinks:native noglob")
        // A parent shim's stale next argument must not extend this invocation.
        .env("CYGSHIM_ARG_11", "stale")
        .output()
        .unwrap();
    assert!(quoted.status.success());
    assert_eq!(
        String::from_utf8(quoted.stdout).unwrap(),
        concat!(
            " '' 'literal \"quote\"' '[brackets]' '{braces}' '*.glob'",
            " 'literal $(printf substituted) value'",
            " 'literal `printf substituted` value'",
            " 'O'\\''Brien' 'literal\\value'\n",
        )
    );
}

#[test]
fn git_converts_only_path_producing_rev_parse_output() {
    let git = env!("CARGO_BIN_EXE_git");
    let repository = TempDirectory::new("[brackets]{braces}");
    let run = |args: &[&str]| {
        Command::new(git)
            .args(args)
            .current_dir(repository.path())
            .output()
            .unwrap()
    };
    let assert_success = |output: &std::process::Output| {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };

    assert_success(&run(&["init", "--quiet", "--initial-branch=main"]));
    assert_success(&run(&[
        "-c",
        "user.name=Cygshim Test",
        "-c",
        "user.email=cygshim@example.com",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "initial",
    ]));
    assert_success(&run(&["update-ref", "refs/remotes/origin/main", "HEAD"]));
    assert_success(&run(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/repository.git",
    ]));
    assert_success(&run(&["config", "branch.main.remote", "origin"]));
    assert_success(&run(&["config", "branch.main.merge", "refs/heads/main"]));

    let native_root = repository.path().to_string_lossy().replace('\\', "/");
    let root = run(&["-c", "core.quotepath=false", "rev-parse", "--show-toplevel"]);
    assert_success(&root);
    assert_eq!(
        String::from_utf8(root.stdout).unwrap(),
        format!("{native_root}\n")
    );

    let paths = run(&["rev-parse", "--show-toplevel", "--absolute-git-dir"]);
    assert_success(&paths);
    assert_eq!(
        String::from_utf8(paths.stdout).unwrap(),
        format!("{native_root}\n{native_root}/.git\n")
    );

    let upstream = run(&["rev-parse", "--symbolic-full-name", "main@{u}"]);
    assert_success(&upstream);
    assert_eq!(
        String::from_utf8(upstream.stdout).unwrap(),
        "refs/remotes/origin/main\n"
    );
}

#[test]
fn git_accepts_arguments_larger_than_cyg_gits_fixed_buffer() {
    let git = env!("CARGO_BIN_EXE_git");
    let argument = "x".repeat(5_000);
    let quoted = Command::new(git)
        .args(["rev-parse", "--sq-quote"])
        .arg(&argument)
        .output()
        .unwrap();

    assert!(
        quoted.status.success(),
        "{}",
        String::from_utf8_lossy(&quoted.stderr)
    );
    assert_eq!(
        String::from_utf8(quoted.stdout).unwrap(),
        format!(" '{argument}'\n")
    );
}
