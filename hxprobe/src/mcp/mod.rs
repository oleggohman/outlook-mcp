//! `hxprobe mcp`: a local, read-only MCP server over the SQLite export of the
//! store.

pub mod attach;
pub mod cache;
pub mod client;
pub mod office;
pub mod proto;
pub mod rpc;
pub mod service;
pub mod text;
pub mod tools;

use std::sync::OnceLock;

/// How [`log`] writes its lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStyle {
    /// `hxprobe mcp: <msg>` (the default).
    Mcp,
    /// `<UTC time> hxprobe service[<pid>]: <msg>`.
    Service,
}

static LOG_STYLE: OnceLock<LogStyle> = OnceLock::new();

/// Set the style of [`log`] for the whole process; only the first call
/// counts.
pub fn set_log_style(style: LogStyle) {
    let _ = LOG_STYLE.set(style);
}

/// Write one log line to stderr in the process's style (see
/// [`set_log_style`]). A failed write (a closed stderr) is ignored, so
/// logging never stops the server.
pub fn log(msg: &str) {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = match LOG_STYLE.get() {
        Some(LogStyle::Service) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            writeln!(
                err,
                "{} hxprobe service[{}]: {msg}",
                text::iso(now),
                std::process::id()
            )
        }
        _ => writeln!(err, "hxprobe mcp: {msg}"),
    };
}
