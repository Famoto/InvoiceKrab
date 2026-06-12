//! `krab-invoice` binary — a thin shell delegating to [`einvoice_interfaces::cli::run`].
//!
//! All logic lives in the [`cli`](einvoice_interfaces::cli) module so it can be
//! unit- and integration-tested without spawning a process; this entry point
//! only wires up the real argv and standard streams and forwards the exit code.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();

    let code = einvoice_interfaces::cli::run(
        &args,
        &mut stdin.lock(),
        &mut stdout.lock(),
        &mut stderr.lock(),
    );

    ExitCode::from(code as u8)
}
