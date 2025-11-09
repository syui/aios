use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// AIOS system configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AIOSConfig {
    #[serde(default)]
    pub aios: SystemConfig,

    #[serde(default)]
    pub llm: LLMConfig,

    #[serde(default)]
    pub agents: AgentsConfig,

    #[serde(default)]
    pub recovery: RecoveryConfig,

    #[serde(default)]
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    #[serde(default = "default_version")]
    pub version: String,

    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    #[serde(default = "default_socket_path")]
    pub socket_path: PathBuf,

    #[serde(default)]
    pub container_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMConfig {
    #[serde(default = "default_provider")]
    pub default_provider: String,

    #[serde(default)]
    pub openai: OpenAIConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIConfig {
    pub api_key: Option<String>,

    #[serde(default = "default_openai_base_url")]
    pub base_url: String,

    #[serde(default = "default_openai_model")]
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentsConfig {
    #[serde(default = "default_true")]
    pub core_agent: bool,

    #[serde(default)]
    pub security_agent: bool,

    #[serde(default)]
    pub system_agent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default = "default_snapshot_interval")]
    pub snapshot_interval: String,

    #[serde(default = "default_max_snapshots")]
    pub max_snapshots: usize,

    #[serde(default)]
    pub auto_rollback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    #[serde(default = "default_security_mode")]
    pub mode: String,

    #[serde(default)]
    pub sandbox: bool,

    #[serde(default)]
    pub dangerous_patterns: Vec<String>,

    #[serde(default)]
    pub require_confirm: Vec<String>,
}

// Default values
fn default_version() -> String {
    "0.1.0".to_string()
}

fn default_data_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/etc"))
        .join("aios")
}

fn default_socket_path() -> PathBuf {
    PathBuf::from("/tmp/aios-runtime.sock")
}

fn default_provider() -> String {
    "openai".to_string()
}

fn default_openai_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

fn default_openai_model() -> String {
    "gpt-4".to_string()
}

fn default_true() -> bool {
    true
}

fn default_snapshot_interval() -> String {
    "hourly".to_string()
}

fn default_max_snapshots() -> usize {
    24
}

fn default_security_mode() -> String {
    "normal".to_string()
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            version: default_version(),
            data_dir: default_data_dir(),
            socket_path: default_socket_path(),
            container_id: None,
        }
    }
}

impl Default for OpenAIConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: default_openai_base_url(),
            model: default_openai_model(),
        }
    }
}

impl Default for LLMConfig {
    fn default() -> Self {
        Self {
            default_provider: default_provider(),
            openai: OpenAIConfig::default(),
        }
    }
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            core_agent: true,
            security_agent: false,
            system_agent: false,
        }
    }
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            snapshot_interval: default_snapshot_interval(),
            max_snapshots: default_max_snapshots(),
            auto_rollback: false,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            mode: default_security_mode(),
            sandbox: false,
            dangerous_patterns: vec![
                "rm -rf /".to_string(),
                "mkfs.*".to_string(),
            ],
            require_confirm: vec![
                "sudo.*".to_string(),
                "systemctl.*".to_string(),
            ],
        }
    }
}

impl Default for AIOSConfig {
    fn default() -> Self {
        Self {
            aios: SystemConfig::default(),
            llm: LLMConfig::default(),
            agents: AgentsConfig::default(),
            recovery: RecoveryConfig::default(),
            security: SecurityConfig::default(),
        }
    }
}

impl AIOSConfig {
    /// Load configuration from file
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read config file: {:?}", path.as_ref()))?;

        let config: AIOSConfig = toml::from_str(&content)
            .context("Failed to parse config file")?;

        Ok(config)
    }

    /// Load configuration from default location or use defaults
    pub fn load_or_default() -> Self {
        let default_path = Self::default_config_path();

        if default_path.exists() {
            Self::load(&default_path).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    /// Get default config file path
    pub fn default_config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("/etc"))
            .join("aios")
            .join("config.toml")
    }

    /// Save configuration to file
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let content = toml::to_string_pretty(self)
            .context("Failed to serialize config")?;

        // Create parent directories
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(path.as_ref(), content)
            .with_context(|| format!("Failed to write config file: {:?}", path.as_ref()))?;

        Ok(())
    }
}
