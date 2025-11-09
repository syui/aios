use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use aios_agents::{Agent, OpenAIProvider};
use aios_config::AIOSConfig;
use aios_memory::MemoryStore;
use aios_recovery::RecoveryManager;
use aios_tools::ToolRegistry;

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

pub struct Daemon {
    config: AIOSConfig,
    agent: Agent,
    recovery: RecoveryManager,
}

impl Daemon {
    pub async fn new() -> Result<Self> {
        // Load configuration
        let config = AIOSConfig::load_or_default();

        tracing::info!("Loaded configuration from {:?}", AIOSConfig::default_config_path());

        // Initialize components
        let memory = MemoryStore::default()?;
        let tools = ToolRegistry::new();
        let recovery = RecoveryManager::default()?;

        // Create LLM provider
        let llm_provider = OpenAIProvider::from_env()
            .context("Failed to create LLM provider. Make sure OPENAI_API_KEY is set.")?;

        // Create agent
        let system_prompt = "You are AIOS, an AI Operating System assistant. \
            You help users manage their system, execute commands, and solve problems. \
            You have access to tools like bash, read, write, and list to interact with the system. \
            Always be helpful, safe, and explain what you're doing.".to_string();

        let agent = Agent::new(
            Box::new(llm_provider),
            tools,
            memory,
            system_prompt,
        );

        tracing::info!("AIOS daemon initialized successfully");

        Ok(Self {
            config,
            agent,
            recovery,
        })
    }

    pub async fn serve(&self) -> Result<()> {
        let socket_path = &self.config.aios.socket_path;

        // Remove old socket if exists
        if socket_path.exists() {
            tokio::fs::remove_file(&socket_path).await?;
        }

        // Create parent directory
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let listener = UnixListener::bind(&socket_path)
            .context("Failed to bind Unix socket")?;

        tracing::info!("AIOS daemon listening on {:?}", socket_path);

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tracing::debug!("New client connected");

                    // Handle each connection in a separate task
                    // Note: In a real implementation, we'd need to handle
                    // the agent mutability properly (e.g., using Arc<Mutex<Agent>>)
                    // For now, this is a simplified version
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream).await {
                            tracing::error!("Connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Accept error: {}", e);
                }
            }
        }
    }

    pub async fn handle_request(&mut self, request: Request) -> Response {
        tracing::info!("Handling request: {}", request.method);

        let result = match request.method.as_str() {
            "chat" => {
                let message = request.params["message"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();

                match self.agent.chat(&message).await {
                    Ok(response) => Ok(serde_json::json!({
                        "response": response
                    })),
                    Err(e) => Err(format!("Agent error: {}", e)),
                }
            }

            "snapshot" => {
                let description = request.params["description"]
                    .as_str()
                    .unwrap_or("Manual snapshot")
                    .to_string();

                match self.recovery.create_snapshot(description).await {
                    Ok(snapshot) => Ok(serde_json::to_value(snapshot).unwrap()),
                    Err(e) => Err(format!("Snapshot error: {}", e)),
                }
            }

            "list_snapshots" => {
                match self.recovery.list_snapshots().await {
                    Ok(snapshots) => Ok(serde_json::to_value(snapshots).unwrap()),
                    Err(e) => Err(format!("List error: {}", e)),
                }
            }

            "restore" => {
                let snapshot_id = request.params["id"]
                    .as_str()
                    .unwrap_or("");

                match self.recovery.restore_snapshot(snapshot_id).await {
                    Ok(_) => Ok(serde_json::json!({
                        "message": "Snapshot restored"
                    })),
                    Err(e) => Err(format!("Restore error: {}", e)),
                }
            }

            "ping" => Ok(serde_json::json!({
                "message": "pong"
            })),

            _ => Err(format!("Unknown method: {}", request.method)),
        };

        match result {
            Ok(value) => Response {
                id: request.id,
                result: Some(value),
                error: None,
            },
            Err(error) => Response {
                id: request.id,
                result: None,
                error: Some(error),
            },
        }
    }
}

async fn handle_connection(mut stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to parse request: {}", e);
                line.clear();
                continue;
            }
        };

        // For this simplified version, we just echo back a response
        // In a real implementation, we'd pass this to the daemon
        let response = Response {
            id: request.id.clone(),
            result: Some(serde_json::json!({
                "message": "Request received (simplified response)"
            })),
            error: None,
        };

        let response_str = serde_json::to_string(&response)? + "\n";
        writer.write_all(response_str.as_bytes()).await?;
        writer.flush().await?;

        line.clear();
    }

    Ok(())
}
