//! Stdio MCP server. Sending mail is not a tool.

#![forbid(unsafe_code)]

fn main() {
    if let Err(err) = fell_mcp::cli_main(std::env::args_os()) {
        eprintln!("fell-mcp: {err}");
        std::process::exit(2);
    }
}
