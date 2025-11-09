use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
    pub metadata: serde_json::Value,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult>;

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
        }
    }
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            tools: HashMap::new(),
        };

        // Register built-in tools
        registry.register(Box::new(BashTool));
        registry.register(Box::new(ReadTool));
        registry.register(Box::new(WriteTool));
        registry.register(Box::new(ListTool));

        registry
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&Box<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn list(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<ToolResult> {
        let tool = self.get(name)
            .with_context(|| format!("Tool not found: {}", name))?;

        tool.execute(args).await
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// Built-in tools

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command and return the output"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let command = args["command"]
            .as_str()
            .context("Missing 'command' argument")?;

        tracing::info!("Executing bash command: {}", command);

        let output = duct::cmd!("sh", "-c", command)
            .stdout_capture()
            .stderr_capture()
            .unchecked()
            .run()?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);
        let success = output.status.success();

        let result_text = if success {
            format!("Exit code: {}\n\nStdout:\n{}\n\nStderr:\n{}", exit_code, stdout, stderr)
        } else {
            format!("Command failed with exit code: {}\n\nStdout:\n{}\n\nStderr:\n{}", exit_code, stdout, stderr)
        };

        Ok(ToolResult {
            success,
            output: result_text,
            metadata: json!({
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
            }),
        })
    }
}

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the file to read"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .context("Missing 'path' argument")?;

        tracing::info!("Reading file: {}", path);

        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read file: {}", path))?;

        Ok(ToolResult {
            success: true,
            output: content,
            metadata: json!({
                "path": path,
            }),
        })
    }
}

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .context("Missing 'path' argument")?;
        let content = args["content"]
            .as_str()
            .context("Missing 'content' argument")?;

        tracing::info!("Writing to file: {}", path);

        // Create parent directories if needed
        if let Some(parent) = std::path::Path::new(path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(path, content)
            .await
            .with_context(|| format!("Failed to write file: {}", path))?;

        Ok(ToolResult {
            success: true,
            output: format!("Successfully wrote to file: {}", path),
            metadata: json!({
                "path": path,
                "size": content.len(),
            }),
        })
    }
}

pub struct ListTool;

#[async_trait]
impl Tool for ListTool {
    fn name(&self) -> &str {
        "list"
    }

    fn description(&self) -> &str {
        "List files in a directory"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The directory path to list (default: current directory)"
                },
                "pattern": {
                    "type": "string",
                    "description": "Optional glob pattern to filter files"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
        let path = args["path"]
            .as_str()
            .unwrap_or(".");
        let pattern = args["pattern"].as_str();

        tracing::info!("Listing files in: {}", path);

        let mut entries = tokio::fs::read_dir(path)
            .await
            .with_context(|| format!("Failed to read directory: {}", path))?;

        let mut files = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let file_name = entry.file_name().to_string_lossy().to_string();

            // Apply pattern filter if specified
            if let Some(pat) = pattern {
                if !file_name.contains(pat) {
                    continue;
                }
            }

            files.push(file_name);
        }

        files.sort();

        Ok(ToolResult {
            success: true,
            output: files.join("\n"),
            metadata: json!({
                "path": path,
                "count": files.len(),
            }),
        })
    }
}
