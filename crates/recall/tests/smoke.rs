//! Step-3 smoke test for the recall skeleton (ADR-0034).
//!
//! Only validates that the `recall` binary builds — `CARGO_BIN_EXE_recall`
//! is set by cargo only when the bin target compiled. Driving the stdio
//! MCP handshake end-to-end (`initialize` → `notifications/initialized`
//! → `tools/list` → assert `ping` registered) lands in the next commit
//! when the six retrieval verbs go in and the test infrastructure for
//! piped JSON-RPC frames is worth its weight.

#![allow(
    unused_crate_dependencies,
    reason = "tests/* binaries see crate-level deps as unused; the integration test only spawns the recall binary via CARGO_BIN_EXE_recall"
)]

#[cfg(test)]
mod tests {
    #[test]
    fn recall_binary_builds() {
        let bin = env!("CARGO_BIN_EXE_recall");
        assert!(!bin.is_empty(), "CARGO_BIN_EXE_recall must be set");
    }
}
