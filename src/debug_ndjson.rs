//! Session debug NDJSON logger (agent instrumentation). Not for production secrets.

use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const SESSION_ID: &str = "716aff";

/// Append one NDJSON debug line for hypothesis testing.
pub fn agent_log(
    hypothesis_id: &str,
    location: &str,
    message: &str,
    data: serde_json::Value,
) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let line = json!({
        "sessionId": SESSION_ID,
        "runId": std::env::var("RTMP2DASH_DEBUG_RUN").unwrap_or_else(|_| "pre-fix".into()),
        "hypothesisId": hypothesis_id,
        "location": location,
        "message": message,
        "data": data,
        "timestamp": ts,
    });
    let payload = match serde_json::to_string(&line) {
        Ok(s) => s,
        Err(_) => return,
    };

    // #region agent log
    for path in debug_log_paths() {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{payload}");
        }
    }
    // #endregion
}

fn debug_log_paths() -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(p) = std::env::var("RTMP2DASH_DEBUG_LOG") {
        paths.push(p);
    } else if cfg!(target_os = "macos") {
        // Local workspace (debug mode) — never write this absolute path on Linux
        // hosts (autofs/missing parents can stall open() and freeze the runtime).
        paths.push(
            "/Users/seantw/case2026/rust/p2p/.cursor/debug-716aff.log".to_string(),
        );
    } else {
        paths.push("/home/rtmp2dash/logs/debug-716aff.log".to_string());
    }
    paths
}
