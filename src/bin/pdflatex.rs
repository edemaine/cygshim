// Include shared sources directly so each shim stays a standalone binary target.
#[path = "../bridge.rs"]
mod bridge;
#[path = "../tex.rs"]
mod tex;

fn main() -> std::process::ExitCode {
    tex::run("/usr/bin/pdflatex")
}
