//! Audit logger for events.jsonl and summary.json.

pub fn redact(input: &str) -> String {
    input.replace("Authorization", "[REDACTED]")
}
