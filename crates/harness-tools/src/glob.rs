use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobInput {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Default)]
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path":    { "type": "string" }
            },
            "required": ["pattern"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<GlobInput>(input.clone()) {
            Ok(gi) => Preview {
                summary_line: format!("Glob {}", gi.pattern),
                detail: gi.path,
            },
            Err(e) => Preview {
                summary_line: "Glob <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, _input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        // Iter 1 body: ignore::WalkBuilder + globset::GlobMatcher.
        Err(ToolError::Other("Glob::call not yet implemented".into()))
    }
}
