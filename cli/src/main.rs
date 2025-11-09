use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "aios")]
#[command(about = "AIOS - AI Operating System", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Send a chat message to the AI
    Chat {
        /// The message to send
        message: String,
    },

    /// Start interactive chat mode
    Shell,

    /// Create a system snapshot
    Snapshot {
        /// Description of the snapshot
        #[arg(short, long)]
        description: Option<String>,
    },

    /// List all snapshots
    Snapshots,

    /// Restore from a snapshot
    Restore {
        /// Snapshot ID to restore
        id: String,
    },

    /// Check daemon status
    Status,

    /// Show system information
    Info,
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    id: String,
    method: String,
    params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    id: String,
    result: Option<serde_json::Value>,
    error: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Chat { message } => {
            let response = send_request("chat", serde_json::json!({
                "message": message
            })).await?;

            if let Some(result) = response.result {
                if let Some(resp_text) = result["response"].as_str() {
                    println!("\n{}\n", resp_text);
                } else {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
            } else if let Some(error) = response.error {
                eprintln!("Error: {}", error);
            }
        }

        Commands::Shell => {
            interactive_shell().await?;
        }

        Commands::Snapshot { description } => {
            let desc = description.unwrap_or_else(|| "Manual snapshot".to_string());
            let response = send_request("snapshot", serde_json::json!({
                "description": desc
            })).await?;

            if let Some(result) = response.result {
                println!("Snapshot created:");
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if let Some(error) = response.error {
                eprintln!("Error: {}", error);
            }
        }

        Commands::Snapshots => {
            let response = send_request("list_snapshots", serde_json::json!({})).await?;

            if let Some(result) = response.result {
                if let Some(snapshots) = result.as_array() {
                    if snapshots.is_empty() {
                        println!("No snapshots found");
                    } else {
                        println!("\nSnapshots:\n");
                        for snapshot in snapshots {
                            println!("ID: {}", snapshot["id"].as_str().unwrap_or(""));
                            println!("Description: {}", snapshot["description"].as_str().unwrap_or(""));
                            println!("Created: {}", snapshot["created_at"].as_str().unwrap_or(""));
                            println!();
                        }
                    }
                } else {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
            } else if let Some(error) = response.error {
                eprintln!("Error: {}", error);
            }
        }

        Commands::Restore { id } => {
            let response = send_request("restore", serde_json::json!({
                "id": id
            })).await?;

            if let Some(result) = response.result {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if let Some(error) = response.error {
                eprintln!("Error: {}", error);
            }
        }

        Commands::Status => {
            match send_request("ping", serde_json::json!({})).await {
                Ok(response) => {
                    if response.result.is_some() {
                        println!("AIOS daemon is running");
                    } else {
                        println!("AIOS daemon responded with error");
                    }
                }
                Err(e) => {
                    eprintln!("AIOS daemon is not running: {}", e);
                    eprintln!("\nTo start the daemon:");
                    eprintln!("  aios-runtime");
                }
            }
        }

        Commands::Info => {
            let config = aios_config::AIOSConfig::load_or_default();
            println!("AIOS System Information\n");
            println!("Version: {}", config.aios.version);
            println!("Data directory: {:?}", config.aios.data_dir);
            println!("Socket path: {:?}", config.aios.socket_path);
            println!("\nLLM Configuration:");
            println!("  Provider: {}", config.llm.default_provider);
            println!("  Model: {}", config.llm.openai.model);
            println!("  Base URL: {}", config.llm.openai.base_url);
            println!("\nRecovery:");
            println!("  Enabled: {}", config.recovery.enabled);
            println!("  Interval: {}", config.recovery.snapshot_interval);
            println!("  Max snapshots: {}", config.recovery.max_snapshots);
        }
    }

    Ok(())
}

async fn send_request(method: &str, params: serde_json::Value) -> Result<Response> {
    let socket_path = aios_config::AIOSConfig::load_or_default().aios.socket_path;

    let mut stream = UnixStream::connect(&socket_path)
        .await
        .context("Failed to connect to AIOS daemon. Is it running?")?;

    let request = Request {
        id: ulid::Ulid::new().to_string(),
        method: method.to_string(),
        params,
    };

    let request_str = serde_json::to_string(&request)? + "\n";
    stream.write_all(request_str.as_bytes()).await?;
    stream.flush().await?;

    let (reader, _) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    reader.read_line(&mut line).await?;

    let response: Response = serde_json::from_str(&line)
        .context("Failed to parse response")?;

    Ok(response)
}

async fn interactive_shell() -> Result<()> {
    println!("AIOS Interactive Shell");
    println!("Type 'exit' or 'quit' to exit\n");

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);

    loop {
        print!("aios> ");
        std::io::Write::flush(&mut std::io::stdout())?;

        let mut input = String::new();
        if reader.read_line(&mut input).await? == 0 {
            break;
        }

        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        if input == "exit" || input == "quit" {
            println!("Goodbye!");
            break;
        }

        match send_request("chat", serde_json::json!({
            "message": input
        })).await {
            Ok(response) => {
                if let Some(result) = response.result {
                    if let Some(resp_text) = result["response"].as_str() {
                        println!("\n{}\n", resp_text);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    }
                } else if let Some(error) = response.error {
                    eprintln!("Error: {}", error);
                }
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    }

    Ok(())
}
