use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteInput {
    pub file_path: String,
    pub content: String,
}

#[derive(Debug, Default)]
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["file_path", "content"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<WriteInput>(input.clone()) {
            Ok(wi) => {
                let bytes = wi.content.len();
                Preview {
                    summary_line: format!("Write {} ({} bytes)", wi.file_path, bytes),
                    detail: None,
                }
            }
            Err(e) => Preview {
                summary_line: "Write <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, _input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        // Iter 1 body: tempfile + renameat2(RENAME_NOREPLACE) on Linux.
        Err(ToolError::Other("Write::call not yet implemented".into()))
    }
}
