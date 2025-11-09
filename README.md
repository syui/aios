# AIOS - AI Operating System

**A complete redesign of AI-powered operating system management**

[![Build Status](https://img.shields.io/badge/build-passing-brightgreen)]()
[![License](https://img.shields.io/badge/license-MIT-blue)]()

## Overview

AIOS is a unified AI Operating System built from scratch with a daemon-first architecture. Unlike the separate aigpt/aishell approach, AIOS integrates everything into a single cohesive system.

## Quick Start

```bash
# Clone and build
git clone https://github.com/syui/aios.git
cd aios
cargo build --release

# Install
./install.sh

# Set API key
export OPENAI_API_KEY="sk-your-key"

# Start daemon
systemctl --user start aios-runtime

# Use AIOS
aios chat "What is my current directory?"
aios shell  # Interactive mode
```

👉 **See [QUICKSTART.md](QUICKSTART.md) for detailed instructions**

## Architecture

```
┌─────────────────────────────────────────┐
│  AIOS - AI Operating System             │
├─────────────────────────────────────────┤
│                                         │
│  Layer 1: Configuration (TOML-based)    │
│  Layer 2: Agent Runtime (daemon)        │
│  Layer 3: Memory & Tools                │
│  Layer 4: Recovery & Safety             │
│  Layer 5: CLI Interface                 │
└─────────────────────────────────────────┘
```

### Components

- **aios-runtime**: Core daemon that runs as a systemd service
- **aios-cli**: Command-line client (`aios` command)
- **aios-memory**: Unified memory system (SQLite + FTS)
- **aios-agents**: LLM agent implementation with multi-provider support
- **aios-tools**: Extensible tool registry (bash, read, write, list, etc.)
- **aios-recovery**: Snapshot and rollback system
- **aios-config**: Declarative TOML configuration

## Installation

```bash
# Build all components
cargo build --release

# Install binaries
cargo install --path runtime --bin aios-runtime
cargo install --path cli --bin aios

# Or use the workspace
cargo install --path .
```

## Configuration

Create `/etc/aios/config.toml`:

```toml
[aios]
version = "0.1.0"
data_dir = "/var/lib/aios"
socket_path = "/tmp/aios-runtime.sock"

[llm]
default_provider = "openai"

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4"

[agents]
core_agent = true
security_agent = false
system_agent = false

[recovery]
enabled = true
snapshot_interval = "hourly"
max_snapshots = 24
auto_rollback = false

[security]
mode = "normal"  # safe, normal, paranoid
sandbox = false
dangerous_patterns = ["rm -rf /", "mkfs.*"]
require_confirm = ["sudo.*", "systemctl.*"]
```

## Usage

### 1. Start the daemon

```bash
# Foreground
export OPENAI_API_KEY="your-key"
aios-runtime

# Or as systemd service
sudo systemctl start aios-runtime
sudo systemctl enable aios-runtime
```

### 2. Interact via CLI

```bash
# One-shot command
aios chat "Install nginx and configure it"

# Interactive shell
aios shell

# Check status
aios status

# System info
aios info
```

### 3. Snapshot management

```bash
# Create snapshot
aios snapshot --description "Before system upgrade"

# List snapshots
aios snapshots

# Restore from snapshot
aios restore <snapshot-id>
```

## Features

### Unified Daemon

- Single process handles all AI interactions
- Unix socket IPC (fast, secure)
- Persistent memory across sessions
- Tool execution with safety checks

### Memory System

- SQLite + Full-Text Search
- Cross-container memory sharing
- Command history and chat logs
- Semantic search capabilities

### Recovery System

- Automatic periodic snapshots
- Manual snapshot creation
- Rollback to any snapshot
- Configurable retention policy

### Declarative Configuration

- TOML-based system config
- Override via environment variables
- Hot-reload support (planned)

## Comparison: Old vs New

| Aspect | Old (aigpt + aishell) | New (AIOS) |
|--------|----------------------|-----------|
| **Architecture** | Separate tools | Unified daemon |
| **IPC** | None (separate processes) | Unix socket |
| **Memory** | aigpt only | Unified across all |
| **Configuration** | Separate configs | Single TOML |
| **Recovery** | Manual | Built-in snapshots |
| **Tools** | Hardcoded | Plugin registry |
| **Deployment** | Two binaries | Daemon + CLI |

## Development

```bash
# Build workspace
cargo build

# Run tests
cargo test

# Check all packages
cargo check --workspace

# Format code
cargo fmt --all

# Run linter
cargo clippy --workspace
```

## Project Structure

```
aios/
├── Cargo.toml              # Workspace definition
├── runtime/                # Core daemon
│   ├── src/
│   │   ├── main.rs
│   │   └── daemon.rs
│   └── Cargo.toml
├── cli/                    # CLI client
│   ├── src/main.rs
│   └── Cargo.toml
├── memory/                 # Memory system
│   ├── src/lib.rs
│   └── Cargo.toml
├── agents/                 # LLM agents
│   ├── src/lib.rs
│   └── Cargo.toml
├── tools/                  # Tool registry
│   ├── src/lib.rs
│   └── Cargo.toml
├── recovery/               # Snapshot system
│   ├── src/lib.rs
│   └── Cargo.toml
└── config/                 # Configuration
    ├── src/lib.rs
    └── Cargo.toml
```

## Roadmap

### Completed ✅
- [x] Core daemon architecture with Arc<Mutex> for concurrent access
- [x] Unified memory system (SQLite + FTS5)
- [x] Tool registry (bash, read, write, list)
- [x] Agent loop implementation with OpenAI
- [x] CLI client (chat, shell, snapshot commands)
- [x] Snapshot & restore system (config + memory backup)
- [x] Systemd user service integration
- [x] Installation script

### In Progress 🚧
- [ ] End-to-end testing
- [ ] Documentation improvements

### Planned 📋
- [ ] Multi-agent coordination
- [ ] Plugin system for custom tools
- [ ] Security sandbox modes
- [ ] Vector database for semantic memory
- [ ] Anthropic Claude support
- [ ] Ollama (local LLM) support
- [ ] Web UI dashboard

## Integration with Containers

AIOS is designed to work seamlessly with containers:

```bash
# In container
machinectl shell aios
export OPENAI_API_KEY="..."
aios-runtime &
aios chat "Configure this container"

# Memory is shared at ~/.config/aios/memory.db
# Accessible from all containers
```

## License

MIT License

## Authors

syui

## Related Projects

- [aigpt](https://github.com/syui/aigpt) - Original memory system
- [aishell](../aishell) - Shell automation tool
