use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrepMode {
    Content,
    FilesWithMatches,
    Count,
}

impl Default for GrepMode {
    fn default() -> Self {
        Self::FilesWithMatches
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepInput {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub output_mode: GrepMode,
}

#[derive(Debug, Default)]
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern":     { "type": "string" },
                "path":        { "type": "string" },
                "glob":        { "type": "string" },
                "output_mode": { "type": "string", "enum": ["content","files_with_matches","count"] }
            },
            "required": ["pattern"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<GrepInput>(input.clone()) {
            Ok(gi) => Preview {
                summary_line: format!("Grep {}", gi.pattern),
                detail: gi.path,
            },
            Err(e) => Preview {
                summary_line: "Grep <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, _input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        // Iter 1 body: grep_searcher + grep_regex; honour output_mode.
        Err(ToolError::Other("Grep::call not yet implemented".into()))
    }
}
