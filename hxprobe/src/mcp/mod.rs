//! `hxprobe mcp`: a local, read-only MCP server over the SQLite export of the
//! store.

pub mod cache;
pub mod rpc;
pub mod text;
pub mod tools;

/// Write one log line, `hxprobe mcp: <msg>`, to stderr. A failed write (a
/// closed stderr) is ignored, so logging never stops the server.
pub fn log(msg: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "hxprobe mcp: {msg}");
}
