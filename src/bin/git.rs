// Include shared sources directly so each shim stays a standalone binary target.
#[path = "../bridge.rs"]
mod bridge;
#[path = "../git.rs"]
mod git;

fn main() -> std::process::ExitCode {
    git::run()
}
