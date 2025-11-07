# <img src="./icon/ai.png" width="30"> ai `os`

**aios** = ArchLinux + Claude Code + aigpt in systemd-nspawn

A minimal ArchLinux environment optimized for Claude Code with shared AI memory.

```
systemd-nspawn container
├── Claude Code (AI interface)
├── aigpt (shared memory)
└── zsh (.zshrc configured)

$ sudo machinectl shell aios
$ claude  # Start Claude Code
```

## Philosophy

**Insert AI into existing flows**

Instead of building a new AI chat interface, use **Claude Code** (which already works).

aios provides:
1. Pre-installed **aigpt** (MCP server for shared memory)
2. Pre-installed **Claude Code** (`npm i -g @anthropic-ai/claude-code`)
3. Environment isolation with **systemd-nspawn**
4. Shared memory across containers

## What's Included

### 1. Claude Code

Pre-installed and ready to use:

```sh
$ claude
# Claude Code starts, with MCP connection to aigpt
> Install rust development environment
✓ Installing rust, rust-analyzer, neovim
```

### 2. aigpt (Shared Memory)

MCP server that provides persistent memory to Claude Code:

```
~/.config/syui/ai/gpt/memory.db (SQLite, WAL mode)
  ↓ bind mount
aios-dev, aios-prod, etc. (all share same DB)
```

AI remembers your preferences across all containers.

### 3. systemd-nspawn

Lightweight container environment:

```sh
$ sudo machinectl shell aios
# Inside container with aigpt + Claude Code
```

Multiple containers can share the same memory.

## Architecture

```
Host
├── ~/.config/syui/ai/gpt/memory.db (shared)
│
└── /var/lib/machines/aios/ (container)
    ├── ArchLinux base
    ├── aigpt (MCP server)
    ├── Claude Code
    ├── .zshrc (aliases: ai=claude)
    └── Bind mount → shared memory
```

## Quick Start

```sh
# 1. Clone repository
$ git clone https://github.com/syui/aios
$ cd aios

# 2. Run installer (creates systemd-nspawn container)
$ sudo ./aios-install.sh

# 3. Enter container
$ sudo machinectl shell aios

# 4. Start Claude Code
$ claude
# or
$ ai
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
├── gpt/memory.db      # Shared memory (SQLite WAL)
├── mcp.json           # MCP server config
└── config.toml        # aios config
```

### MCP Configuration

Claude Code connects to aigpt via MCP:

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

This enables Claude Code to use aigpt's memory system.

## Building from Source

```sh
$ pacman -S base-devel archiso docker git rust nodejs npm
$ ./build.zsh
# Creates: aios-bootstrap.tar.gz
```

## How It Works

1. **systemd-nspawn** provides lightweight containers
2. **aigpt** runs as MCP server, stores memories in SQLite
3. **Claude Code** connects to aigpt via MCP
4. Shared memory (`~/.config/syui/ai/gpt/memory.db`) is bind-mounted

**Result:** Claude Code can remember your preferences across all containers.

## Why Not Just Use Claude Code?

You can! aios just provides:
- Pre-configured environment
- Shared memory (aigpt) pre-installed
- Container isolation
- Easy multi-environment setup

## Links

- Repository: https://github.com/syui/aios
- Git: https://git.syui.ai/ai/os
- aigpt: https://git.syui.ai/ai/gpt
- Container: https://git.syui.ai/ai/-/packages/container/os

## Philosophy

**Insert AI into existing flows**

Don't build a new AI chat interface. Use Claude Code (which already works).

Don't create a new container system. Use systemd-nspawn (lightweight, standard).

Just provide:
1. aigpt for shared memory
2. Pre-configured environment
3. Automation scripts

Simple. Minimal. Effective.

---

© syui
