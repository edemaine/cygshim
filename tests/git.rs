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
fn git_preserves_arguments_and_reports_native_repository_paths() {
    let git = env!("CARGO_BIN_EXE_git");
    let repository = TempDirectory::new("[brackets]{braces}");
    let init = Command::new(git)
        .args(["init", "--quiet"])
        .current_dir(repository.path())
        .status()
        .unwrap();
    assert!(init.success());

    let root = Command::new(git)
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(repository.path())
        .output()
        .unwrap();
    assert!(root.status.success());
    assert_eq!(
        String::from_utf8(root.stdout).unwrap().trim(),
        repository.path().to_string_lossy().replace('\\', "/")
    );

    let quoted = Command::new(git)
        .args([
            "rev-parse",
            "--sq-quote",
            "",
            "literal \"quote\"",
            "[brackets]",
            "{braces}",
            "*.glob",
        ])
        .env("CYGWIN", "winsymlinks:native noglob")
        .output()
        .unwrap();
    assert!(quoted.status.success());
    assert_eq!(
        String::from_utf8(quoted.stdout).unwrap(),
        " '' 'literal \"quote\"' '[brackets]' '{braces}' '*.glob'\n"
    );
}
