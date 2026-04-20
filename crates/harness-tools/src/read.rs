use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_READ_LINES: u64 = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadInput {
    pub file_path: String,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Default)]
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path to read" },
                "offset": { "type": "integer", "minimum": 1 },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_READ_LINES }
            },
            "required": ["file_path"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<ReadInput>(input.clone()) {
            Ok(ri) => Preview {
                summary_line: format!("Read {}", ri.file_path),
                detail: None,
            },
            Err(e) => Preview {
                summary_line: "Read <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, _input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        // Iter 1 body: mmap + cat -n + binary sniff + 20k cap; path via fs_safe.
        Err(ToolError::Other("Read::call not yet implemented".into()))
    }
}
