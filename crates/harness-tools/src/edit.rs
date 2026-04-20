use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditInput {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Default)]
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean", "default": false }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<EditInput>(input.clone()) {
            Ok(ei) => Preview {
                summary_line: format!(
                    "Edit {} ({})",
                    ei.file_path,
                    if ei.replace_all { "all" } else { "unique" }
                ),
                detail: None,
            },
            Err(e) => Preview {
                summary_line: "Edit <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, _input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        // Iter 1 body: exact replace + unique-check + replace_all + unified diff.
        Err(ToolError::Other("Edit::call not yet implemented".into()))
    }
}
