# <img src="./icon/ai.png" width="30"> ai `os`

**aios** = AI-managed OS with shared memory

An ArchLinux-based OS where AI conversation interface replaces the traditional shell.

```
User → AI Chat → Commands → Execution
          ↓
      aigpt (shared memory)
          ↓
    systemd-nspawn (isolated environment)
```

## Philosophy

**Insert AI into existing flows**

- Traditional: `User → Shell → Commands`
- aios: `User → AI Chat → Commands`

Simply insert AI layer into the existing workflow.

## Core Features

### 1. AI-First Interface

Default interface is AI conversation, not shell.

```sh
> Install rust development environment
✓ Installing rust, rust-analyzer, neovim
✓ Done

> What did I install yesterday?
Yesterday you installed Python with poetry.
```

### 2. Shared Memory (aigpt)

All containers share the same memory database.

```
Host: ~/.config/syui/ai/gpt/memory.db (shared)
  ↓
aios-dev  → bind mount → same DB
aios-prod → bind mount → same DB
```

AI learns from all environments and remembers your preferences.

### 3. Environment Isolation

Execution environments are isolated using systemd-nspawn.

```sh
# Development environment
$ systemd-nspawn --machine=aios-dev

# Production environment
$ systemd-nspawn --machine=aios-prod
```

Memory is shared, but environments are separated.

## Architecture

```
aios (ArchLinux base)
├── aigpt (memory system)
│   ├── SQLite with WAL mode
│   ├── Layer 3: Personality analysis
│   └── Layer 4: Relationship inference
├── MCP (AI connection standard)
│   └── Claude Code / ChatGPT / Custom AI
├── systemd-nspawn (container runtime)
│   └── Shared memory bind mount
└── Permission system
    ├── Auto-allow
    ├── Notify
    ├── Require approval
    └── Deny
```

## Quick Start

### Installation

```sh
# Clone repository
$ git clone https://github.com/syui/aios
$ cd aios

# Run installer
$ sudo ./aios-install.sh
```

### Usage

```sh
# Start aios container
$ sudo systemctl start systemd-nspawn@aios

# Enter aios shell
$ sudo machinectl shell aios

# Inside aios, AI chat interface starts
[aios] >
```

## Container Distribution

Pre-built containers are available:

```sh
# Docker
$ docker run -it git.syui.ai/ai/os
$ docker run -it ghcr.io/syui/aios

# Podman
$ podman pull aios  # using shortname alias
```

## Configuration

### Directory Structure

```
~/.config/syui/ai/
├── gpt/
│   ├── memory.db       # Shared memory (SQLite WAL)
│   ├── memory.db-wal
│   └── memory.db-shm
├── mcp.json           # MCP server configuration
└── config.toml        # aios configuration
```

### MCP Configuration

`~/.config/syui/ai/mcp.json`:

```json
{
  "mcpServers": {
    "aigpt": {
      "command": "aigpt",
      "args": ["server", "--enable-layer4"]
    }
  }
}
```

### Permission System

`~/.config/syui/ai/config.toml`:

```toml
[permissions]
# Auto-allow (no approval)
auto_allow = ["pacman -Q*", "ls", "cat"]

# Notify (log only)
notify = ["pacman -S*", "git clone*"]

# Require approval
require_approval = ["rm -rf*", "systemctl stop*"]

# Deny
deny = ["rm -rf /", "mkfs*"]
```

## Building from Source

```sh
# Install dependencies
$ pacman -S base-devel archiso docker git rust

# Build bootstrap image
$ ./build.zsh

# Result: aios-bootstrap.tar.gz
```

## Integration with aigpt

aios is designed to work with [aigpt](https://git.syui.ai/ai/gpt) (AI memory system).

aigpt provides:
- **Layer 1**: Memory storage
- **Layer 2**: Priority scoring
- **Layer 3**: Personality analysis (Big Five)
- **Layer 4**: Relationship inference

All memories are shared across containers through bind-mounted SQLite database.

## Comparison

| Aspect | Traditional OS | aios |
|--------|---------------|------|
| Interface | Shell (bash/zsh) | AI Chat |
| Command | Memorize syntax | Natural language |
| Configuration | Manual editing | AI executes |
| Learning | No | Yes (aigpt) |
| Memory | No | Shared (SQLite) |
| Isolation | Docker/Podman | systemd-nspawn |

## Links

- Repository: https://github.com/syui/aios
- Git: https://git.syui.ai/ai/os
- aigpt: https://git.syui.ai/ai/gpt
- Container: https://git.syui.ai/ai/-/packages/container/os

## Philosophy Detail

From conversation with AI about aigpt:

> "What is the essence of this design?"
> "Simply insert AI into existing flows"
>
> - aigpt: Insert AI between conversation and memory
> - aios: Insert AI between user and commands
>
> Not building something entirely new.
> Just adding an AI layer to existing workflows.
> And prepare the environment for that.

This is aios.

---

© syui
