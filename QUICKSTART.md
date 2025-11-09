# AIOS Quick Start Guide

## Installation

### 1. Clone and Build

```bash
git clone https://github.com/syui/aios.git
cd aios
cargo build --release
```

### 2. Install

```bash
chmod +x install.sh
./install.sh
```

This will:
- Build release binaries
- Install `aios` and `aios-runtime` to `~/.local/bin`
- Set up systemd user service
- Enable auto-start on login

## Configuration

### Set up OpenAI API Key

```bash
export OPENAI_API_KEY="sk-your-api-key-here"
```

Add to `~/.bashrc` or `~/.zshrc` to persist:

```bash
echo 'export OPENAI_API_KEY="sk-your-api-key-here"' >> ~/.bashrc
```

### Optional: Custom Configuration

Create `~/.config/aios/config.toml`:

```toml
[aios]
version = "0.1.0"
socket_path = "/tmp/aios-runtime.sock"

[llm]
default_provider = "openai"

[llm.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4"

[recovery]
enabled = true
snapshot_interval = "hourly"
max_snapshots = 24

[security]
mode = "normal"
dangerous_patterns = ["rm -rf /", "mkfs.*"]
require_confirm = ["sudo.*", "systemctl.*"]
```

## Usage

### Start the Daemon

```bash
systemctl --user start aios-runtime
```

Check status:

```bash
systemctl --user status aios-runtime
```

View logs:

```bash
journalctl --user -u aios-runtime -f
```

### Using the CLI

#### One-shot commands:

```bash
# Ask a question
aios chat "What is my current directory?"

# System management
aios chat "Show me disk usage"

# Installation tasks
aios chat "Install nginx and configure it"
```

#### Interactive shell:

```bash
aios shell
```

Then type commands:

```
aios> What files are in my home directory?
aios> Create a Python script that prints hello world
aios> exit
```

### Snapshot Management

#### Create a snapshot:

```bash
aios snapshot --description "Before system upgrade"
```

#### List snapshots:

```bash
aios snapshots
```

#### Restore from snapshot:

```bash
aios restore <snapshot-id>
```

### Check Status

```bash
# Check daemon status
aios status

# View system info
aios info
```

## Examples

### Example 1: System Information

```bash
$ aios chat "Show system information"
```

The AI will use available tools (bash, read, list) to gather system info.

### Example 2: File Management

```bash
$ aios chat "List all Python files in the current directory"
```

### Example 3: Safe Operations

```bash
# Create snapshot before dangerous operation
$ aios snapshot --description "Before cleanup"

# Perform operation
$ aios chat "Clean up old log files"

# If something goes wrong, restore
$ aios snapshots
$ aios restore 01HJ5K6M7N8P9Q0R1S2T3U
```

## Troubleshooting

### Daemon won't start

Check if OPENAI_API_KEY is set:

```bash
echo $OPENAI_API_KEY
```

Check logs:

```bash
journalctl --user -u aios-runtime -n 50
```

### Connection refused

Make sure daemon is running:

```bash
systemctl --user status aios-runtime
```

Check socket file exists:

```bash
ls -la /tmp/aios-runtime.sock
```

### API errors

Verify your OpenAI API key is valid and has credits.

## Advanced

### Running without systemd

```bash
# In one terminal
OPENAI_API_KEY="sk-..." aios-runtime

# In another terminal
aios chat "Hello"
```

### Using with containers

AIOS works seamlessly in containers with shared memory:

```bash
# Bind mount the config directory
docker run -it \
  -v ~/.config/aios:/root/.config/aios \
  -e OPENAI_API_KEY \
  your-container \
  aios chat "Hello from container"
```

## Uninstall

```bash
# Stop and disable service
systemctl --user stop aios-runtime
systemctl --user disable aios-runtime

# Remove binaries
rm ~/.local/bin/aios ~/.local/bin/aios-runtime

# Remove service file
rm ~/.config/systemd/user/aios-runtime.service

# Optional: Remove data
rm -rf ~/.config/aios
```

## Next Steps

- Read the [DESIGN.md](DESIGN.md) for architecture details
- Check [README.md](README.md) for development info
- Report issues on GitHub
