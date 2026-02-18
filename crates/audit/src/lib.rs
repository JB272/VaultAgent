use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AuditEvent<'a, T: Serialize> {
    pub ts_ms: u128,
    pub kind: &'a str,
    pub payload: T,
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub steps: u32,
    pub tool_calls: u32,
    pub policy_violations: u32,
    pub errors: u32,
    pub final_status: String,
}

pub struct AuditWriter {
    run_dir: PathBuf,
    events: File,
}

impl AuditWriter {
    pub fn new(runs_dir: &Path, run_id: &str) -> anyhow::Result<Self> {
        let run_dir = runs_dir.join(run_id);
        fs::create_dir_all(&run_dir)?;
        let events_path = run_dir.join("events.jsonl");
        let events = File::create(events_path)?;
        Ok(Self { run_dir, events })
    }

    pub fn event<T: Serialize>(&mut self, kind: &str, payload: &T) -> anyhow::Result<()> {
        let event = AuditEvent {
            ts_ms: now_ms(),
            kind,
            payload,
        };
        let line = serde_json::to_string(&event)?;
        writeln!(self.events, "{line}")?;
        Ok(())
    }

    pub fn write_summary(&self, summary: &Summary) -> anyhow::Result<()> {
        let path = self.run_dir.join("summary.json");
        let json = serde_json::to_string_pretty(summary)?;
        fs::write(path, json).context("failed writing summary.json")
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }
}

pub fn redact(input: &str) -> String {
    let mut out = input.to_string();
    for marker in ["Authorization:", "Bearer ", "api_key", "token", "secret"] {
        if out.contains(marker) {
            out = out.replace(marker, "[REDACTED]");
        }
    }
    out
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
