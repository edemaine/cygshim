#![cfg(windows)]

use flate2::read::GzDecoder;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new(name: &str) -> Self {
        for index in 0..100 {
            let path =
                std::env::temp_dir().join(format!("cygshim-{name}-{}-{index}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("could not create {}: {error}", path.display()),
            }
        }
        panic!("could not find an unused temporary directory");
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
#[cfg_attr(
    not(feature = "cygwin-pdflatex-test"),
    ignore = "requires Cygwin pdflatex and TeX packages"
)]
fn pdflatex_rewrites_synctex_without_enabling_recorder() {
    let directory = TempDirectory::new("pdflatex path (v1)");
    let source = directory.path().join("document name (draft).tex");
    let output_directory = directory.path().join("output path (v2)");
    fs::create_dir(&output_directory).unwrap();
    fs::write(&source, include_bytes!("fixtures/document.tex")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pdflatex"))
        .args(["-interaction=nonstopmode", "-synctex=1"])
        .arg(path_option("-output-directory=", &output_directory))
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let base = output_directory.join("document name (draft)");
    assert!(!base.with_extension("fls").exists());
    assert_native_source_path(&output.stdout, &source);
    assert_native_source_path(&fs::read(base.with_extension("log")).unwrap(), &source);
    assert_native_synctex(&base.with_extension("synctex.gz"), &source);
}

#[test]
#[cfg_attr(
    not(feature = "cygwin-latexmk-test"),
    ignore = "requires Cygwin latexmk and TeX packages"
)]
fn latexmk_uses_its_recorder_output_to_rewrite_log_paths() {
    let directory = TempDirectory::new("latexmk path (v1)");
    let source = directory.path().join("document name (draft).tex");
    let output_directory = directory.path().join("output path (v2)");
    fs::create_dir(&output_directory).unwrap();
    fs::write(&source, include_bytes!("fixtures/document.tex")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_latexmk"))
        .args(["-pdf", "-interaction=nonstopmode", "-synctex=1"])
        .arg(path_option("-outdir=", &output_directory))
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let base = output_directory.join("document name (draft)");
    assert!(base.with_extension("fls").exists());
    assert_native_source_path(&output.stdout, &source);
    let log = fs::read(base.with_extension("log")).unwrap();
    assert_native_source_path(&log, &source);
    for prefix in [b"(/usr/".as_slice(), b"</usr/", b"{/var/"] {
        assert!(!log.windows(prefix.len()).any(|window| window == prefix));
    }
    assert_native_synctex(&base.with_extension("synctex.gz"), &source);
}

fn path_option(prefix: &str, path: &Path) -> OsString {
    let mut option = OsString::from(prefix);
    option.push(path);
    option
}

fn assert_native_source_path(contents: &[u8], source: &Path) {
    let contents = String::from_utf8_lossy(contents);
    let source = source.to_string_lossy().replace('\\', "/");
    assert!(contents.contains(&source), "missing native path {source}");
    assert!(!contents.contains("/cygdrive/"));
}

fn assert_native_synctex(path: &Path, source: &Path) {
    let mut contents = String::new();
    GzDecoder::new(File::open(path).unwrap())
        .read_to_string(&mut contents)
        .unwrap();
    let input_paths = contents
        .lines()
        .filter_map(|line| {
            line.strip_prefix("Input:")
                .and_then(|line| line.split_once(':'))
                .map(|(_, path)| path)
        })
        .collect::<Vec<_>>();
    let source = source.to_string_lossy().replace('\\', "/");
    assert!(
        input_paths.contains(&source.as_str()),
        "SyncTeX is missing native source path {source}"
    );
    for path in input_paths {
        assert!(!path.starts_with('/'), "SyncTeX retained POSIX path {path}");
    }
}
