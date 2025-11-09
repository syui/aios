use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::env;

use aios_memory::{Memory, MemoryStore, MemoryType};
use aios_tools::{ToolDefinition, ToolRegistry};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub finish_reason: String,
}

#[async_trait]
pub trait LLMProvider: Send + Sync {
    async fn chat(&self, messages: Vec<Message>, tools: Option<Vec<ToolDefinition>>) -> Result<ChatResponse>;
    fn model_name(&self) -> &str;
}

pub struct OpenAIProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl OpenAIProvider {
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            base_url,
            model,
        }
    }

    pub fn from_env() -> Result<Self> {
        let api_key = env::var("OPENAI_API_KEY")
            .context("OPENAI_API_KEY environment variable not set")?;

        let base_url = env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

        let model = env::var("OPENAI_MODEL")
            .unwrap_or_else(|_| "gpt-4".to_string());

        Ok(Self::new(api_key, base_url, model))
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinitionAPI>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

#[derive(Debug, Serialize)]
struct ToolDefinitionAPI {
    #[serde(rename = "type")]
    tool_type: String,
    function: FunctionDefinitionAPI,
}

#[derive(Debug, Serialize)]
struct FunctionDefinitionAPI {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
    finish_reason: String,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

#[async_trait]
impl LLMProvider for OpenAIProvider {
    async fn chat(&self, messages: Vec<Message>, tools: Option<Vec<ToolDefinition>>) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);

        let tool_choice = if tools.is_some() {
            Some("auto".to_string())
        } else {
            None
        };

        let api_tools = tools.map(|ts| {
            ts.iter().map(|t| ToolDefinitionAPI {
                tool_type: "function".to_string(),
                function: FunctionDefinitionAPI {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            }).collect()
        });

        let request = ChatRequest {
            model: self.model.clone(),
            messages,
            tools: api_tools,
            tool_choice,
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .context("Failed to send request to OpenAI API")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI API error ({}): {}", status, error_text);
        }

        let completion: ChatCompletionResponse = response
            .json()
            .await
            .context("Failed to parse OpenAI API response")?;

        let choice = completion
            .choices
            .into_iter()
            .next()
            .context("No choices in response")?;

        Ok(ChatResponse {
            content: choice.message.content.unwrap_or_default(),
            tool_calls: choice.message.tool_calls,
            finish_reason: choice.finish_reason,
        })
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

pub struct Agent {
    llm: Box<dyn LLMProvider>,
    tools: ToolRegistry,
    memory: MemoryStore,
    messages: Vec<Message>,
    system_prompt: String,
}

impl Agent {
    pub fn new(
        llm: Box<dyn LLMProvider>,
        tools: ToolRegistry,
        memory: MemoryStore,
        system_prompt: String,
    ) -> Self {
        let messages = vec![Message {
            role: Role::System,
            content: system_prompt.clone(),
            tool_calls: None,
            tool_call_id: None,
        }];

        Self {
            llm,
            tools,
            memory,
            messages,
            system_prompt,
        }
    }

    pub async fn chat(&mut self, user_input: &str) -> Result<String> {
        // Add user message
        self.messages.push(Message {
            role: Role::User,
            content: user_input.to_string(),
            tool_calls: None,
            tool_call_id: None,
        });

        // Save to memory
        self.memory.create(&Memory::new(user_input, MemoryType::Chat))?;

        let tool_defs = self.tools.list();

        // Agent loop
        let max_iterations = 10;
        for iteration in 0..max_iterations {
            tracing::debug!("Agent iteration {}", iteration + 1);

            let response = self
                .llm
                .chat(self.messages.clone(), Some(tool_defs.clone()))
                .await?;

            // If there are tool calls, execute them
            if let Some(tool_calls) = response.tool_calls {
                tracing::info!("Agent requested {} tool calls", tool_calls.len());

                // Add assistant message with tool calls
                self.messages.push(Message {
                    role: Role::Assistant,
                    content: response.content.clone(),
                    tool_calls: Some(tool_calls.clone()),
                    tool_call_id: None,
                });

                // Execute each tool call
                for tool_call in tool_calls {
                    let tool_name = &tool_call.function.name;
                    let tool_args: serde_json::Value = serde_json::from_str(&tool_call.function.arguments)?;

                    tracing::info!("Executing tool: {}", tool_name);

                    let result = self.tools.execute(tool_name, tool_args).await?;

                    // Save command to memory
                    self.memory.create(
                        &Memory::new(&tool_call.function.arguments, MemoryType::Command)
                            .with_metadata(serde_json::json!({
                                "tool": tool_name,
                                "result": result.output,
                            }))
                    )?;

                    // Add tool result message
                    self.messages.push(Message {
                        role: Role::Tool,
                        content: result.output,
                        tool_calls: None,
                        tool_call_id: Some(tool_call.id.clone()),
                    });
                }

                // Continue loop to get next response
                continue;
            }

            // No tool calls, agent is done
            if !response.content.is_empty() {
                self.messages.push(Message {
                    role: Role::Assistant,
                    content: response.content.clone(),
                    tool_calls: None,
                    tool_call_id: None,
                });

                // Save to memory
                self.memory.create(&Memory::new(&response.content, MemoryType::Chat))?;

                return Ok(response.content);
            }

            break;
        }

        Ok("Agent completed without response".to_string())
    }

    pub fn reset(&mut self) {
        self.messages = vec![Message {
            role: Role::System,
            content: self.system_prompt.clone(),
            tool_calls: None,
            tool_call_id: None,
        }];
    }
}
