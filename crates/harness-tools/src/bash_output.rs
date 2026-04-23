//! `BashOutput` tool — incremental drain of a background shell's output.
//! PLAN §3.2.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bg_registry::{BgRegistry, JobStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashOutputInput {
    pub shell_id: String,
    /// Optional regex applied **per line** to both stdout and stderr.
    /// Lines that don't match are stripped before returning.
    #[serde(default)]
    pub filter: Option<String>,
}

#[derive(Debug, Default)]
pub struct BashOutputTool;

#[async_trait]
impl Tool for BashOutputTool {
    fn name(&self) -> &str {
        "BashOutput"
    }

    fn description(&self) -> &'static str {
        "Fetch new stdout/stderr lines produced by a background Bash job since the last poll."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "shell_id": { "type": "string", "description": "ID returned by Bash(run_in_background=true)" },
                "filter":   { "type": "string", "description": "Optional regex; only matching lines are returned" }
            },
            "required": ["shell_id"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<BashOutputInput>(input.clone()) {
            Ok(bo) => {
                let f = bo.filter.as_deref().unwrap_or("");
                let summary = if f.is_empty() {
                    format!("BashOutput shell_id={}", bo.shell_id)
                } else {
                    format!("BashOutput shell_id={} filter={f}", bo.shell_id)
                };
                Preview {
                    summary_line: summary,
                    detail: None,
                }
            }
            Err(e) => Preview {
                summary_line: "BashOutput <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let parsed: BashOutputInput = serde_json::from_value(input)
            .map_err(|e| ToolError::Validation(format!("BashOutput input: {e}")))?;

        let registry = BgRegistry::global();
        // The turn loop wraps `Err` into `is_error: true` — exactly what we
        // want for an unknown shell_id.
        let Ok((drained, status)) = registry.drain_new_output(&parsed.shell_id) else {
            return Err(ToolError::Validation(format!(
                "unknown shell_id: {}",
                parsed.shell_id
            )));
        };

        let stdout_str = String::from_utf8_lossy(&drained.stdout).into_owned();
        let stderr_str = String::from_utf8_lossy(&drained.stderr).into_owned();

        let (stdout_str, stderr_str) = if let Some(pat) = &parsed.filter {
            let re = regex::Regex::new(pat)
                .map_err(|e| ToolError::Validation(format!("filter regex: {e}")))?;
            (
                filter_lines(&stdout_str, &re),
                filter_lines(&stderr_str, &re),
            )
        } else {
            (stdout_str, stderr_str)
        };

        let status_label = match &status {
            JobStatus::Running { pid } => format!("running pid={pid}"),
            JobStatus::Exited { code, .. } => match code {
                Some(c) => format!("exited code={c}"),
                None => "exited code=?".to_string(),
            },
            JobStatus::Killed { .. } => "killed".to_string(),
        };

        use std::fmt::Write as _;
        let mut summary = String::new();
        let _ = writeln!(
            summary,
            "shell_id={} status={status_label}",
            parsed.shell_id
        );
        if !stdout_str.is_empty() {
            summary.push_str("--- stdout ---\n");
            summary.push_str(&stdout_str);
            if !stdout_str.ends_with('\n') {
                summary.push('\n');
            }
        }
        if !stderr_str.is_empty() {
            summary.push_str("--- stderr ---\n");
            summary.push_str(&stderr_str);
            if !stderr_str.ends_with('\n') {
                summary.push('\n');
            }
        }
        if stdout_str.is_empty() && stderr_str.is_empty() {
            summary.push_str("(no new output)\n");
        }

        Ok(ToolOutput {
            summary,
            detail_path: None,
            stream: None,
        })
    }
}

fn filter_lines(text: &str, re: &regex::Regex) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        // Match against the line minus a trailing '\n' if present.
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        if re.is_match(trimmed) {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_keeps_only_matching_lines() {
        let re = regex::Regex::new("^error").unwrap();
        let got = filter_lines("ok\nerror: x\nok\n", &re);
        assert_eq!(got, "error: x\n");
    }
}
