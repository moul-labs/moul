#![doc = include_str!("../README.md")]

#[cfg(not(target_arch = "wasm32"))]
mod cli;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    cli::main()
}

/// The CLI reads files and opens network connections, so it builds only for
/// native targets; the browser build of the workspace gets this stub.
#[cfg(target_arch = "wasm32")]
fn main() {}
