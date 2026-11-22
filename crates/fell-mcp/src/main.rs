//! Stdio MCP server. Sending mail is not a tool.

#![forbid(unsafe_code)]

use std::io::{self, Write as _};

fn main() {
    if fell_core::diagnostics::init().is_err() {
        let _ = writeln!(io::stderr(), "Fell could not initialize its diagnostic log");
    }
    if let Err(err) = fell_mcp::cli_main(std::env::args_os()) {
        fell_core::diagnostic!(WARN, "MCP command failed");
        let _ = writeln!(io::stderr(), "fell-mcp: {err}");
        std::process::exit(2);
    }
}
