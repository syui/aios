# AIOS Design Document

## Philosophy

**"AI judges, tool executes"** - The AI makes decisions, tools perform actions.

AIOS represents a complete architectural redesign of AI-powered operating system management, learning from aigpt and aishell but built from scratch with a unified vision.

## Why a New Design?

### Problems with the Old Approach (aigpt + aishell)

1. **Fragmentation**: Two separate tools (aigpt, aishell) with different purposes
2. **No Coordination**: They don't communicate or share state
3. **Duplication**: Both have their own configuration, memory, tool systems
4. **Complexity**: Users need to manage multiple processes
5. **Limited Recovery**: No built-in snapshot/rollback system

### AIOS Solution

**One daemon, one vision, one system.**

```
Old:  [aigpt] <-user-> [aishell]
New:  [aios-daemon] <-unix socket-> [aios cli/desktop/API]
```

## Core Design Principles

### 1. Daemon-First Architecture

Everything runs through a single `aios-runtime` daemon:

```
┌──────────────────────────────────┐
│      aios-runtime daemon         │
│  ┌────────────────────────────┐  │
│  │  Agent Loop                │  │
│  │  - LLM Integration         │  │
│  │  - Memory Management       │  │
│  │  - Tool Execution          │  │
│  │  - Recovery Handling       │  │
│  └────────────────────────────┘  │
│           ↕ Unix Socket          │
└──────────────────────────────────┘
         ↕                ↕
    [aios CLI]      [Desktop Apps]
```

**Benefits**:
- Single source of truth
- Consistent state
- Fast IPC (Unix socket)
- Easy monitoring (one process)
- systemd integration

### 2. Unified Memory

One SQLite database for everything:

```rust
pub enum MemoryType {
    Command,    // Shell commands executed
    Chat,       // User conversations
    System,     // System events
    Snapshot,   // Recovery snapshots
}
```

**Shared across**:
- All containers
- All sessions
- All agents

**Full-text search** via SQLite FTS5 for semantic queries.

### 3. Declarative Configuration

Single TOML file describes the entire system:

```toml
[aios]
version = "0.1.0"

[llm]
default_provider = "openai"

[recovery]
snapshot_interval = "hourly"
max_snapshots = 24
```

**Compare to old**:
- Old: 2+ config files, environment variables, command-line flags
- New: 1 config file, consistent override hierarchy

### 4. Recovery-First

Every dangerous operation can be snapshot + rollback:

```rust
recovery.safe_execute("Install nginx", || {
    tools.execute("bash", json!({
        "command": "sudo pacman -S nginx"
    }))
})?;

// Auto-creates snapshot before execution
// Can rollback if failed
```

### 5. Plugin Architecture

Tools are dynamically registered:

```rust
let mut tools = ToolRegistry::new();

// Built-in
tools.register(Box::new(BashTool));
tools.register(Box::new(ReadTool));

// Custom plugins
tools.register(Box::new(GitPlugin));
tools.register(Box::new(DockerPlugin));
```

## Component Design

### aios-runtime (Daemon)

**Responsibilities**:
- Accept connections via Unix socket
- Manage agent lifecycle
- Execute tools safely
- Handle recovery operations
- Maintain memory state

**Architecture**:
```rust
pub struct Daemon {
    config: AIOSConfig,
    agent: Agent,
    recovery: RecoveryManager,
}

impl Daemon {
    pub async fn serve(&self) -> Result<()> {
        let listener = UnixListener::bind(socket_path)?;

        loop {
            let (stream, _) = listener.accept().await?;
            // Handle request
        }
    }
}
```

### aios-cli (Client)

**Responsibilities**:
- Parse user commands
- Connect to daemon
- Display responses

**Commands**:
```bash
aios chat "message"       # One-shot
aios shell                # Interactive
aios snapshot             # Recovery
aios snapshots            # List
aios restore <id>         # Rollback
aios status               # Daemon status
aios info                 # System info
```

### aios-memory

**Schema**:
```sql
CREATE TABLE memories (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    memory_type TEXT NOT NULL,
    metadata TEXT,
    created_at TEXT NOT NULL
);

CREATE VIRTUAL TABLE memories_fts
USING fts5(id, content);
```

**Features**:
- ULID-based IDs (time-sortable)
- JSON metadata
- Full-text search
- Automatic cleanup

### aios-agents

**Agent Loop**:
```
1. User input
2. LLM processes (with tools available)
3. If LLM requests tool:
   a. Execute tool
   b. Send result back to LLM
   c. Repeat
4. Return final response to user
```

**Multi-Agent Support** (future):
```rust
pub trait Agent {
    fn expertise(&self) -> Vec<Domain>;
    async fn handle(&self, task: Task) -> Result<Response>;
}

// Specialized agents
let security_agent = SecurityAgent::new(llm);
let system_agent = SystemAgent::new(llm);
let coordinator = Coordinator::new(vec![security_agent, system_agent]);
```

### aios-tools

**Tool Interface**:
```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

    async fn execute(&self, args: serde_json::Value)
        -> Result<ToolResult>;
}
```

**Built-in Tools**:
- `bash`: Execute shell commands
- `read`: Read files
- `write`: Write files
- `list`: List directory

**Future Tools**:
- `git_*`: Git operations
- `docker_*`: Container management
- `systemd_*`: Service control
- `package_*`: Package management

### aios-recovery

**Snapshot Strategy**:
```
~/.config/aios/snapshots/
├── 01HJ5K6M7N8P9Q0R1S2T3U/
│   ├── metadata.json
│   └── data/
├── 01HJ5K6M7N8P9Q0R1S2T3V/
└── ...
```

**Metadata**:
```json
{
    "id": "01HJ5K6M7N8P9Q0R1S2T3U",
    "description": "Before nginx installation",
    "created_at": "2025-11-09T12:00:00Z",
    "metadata": {
        "trigger": "manual",
        "command": "pacman -S nginx"
    }
}
```

**Policies**:
- Hourly automatic snapshots
- Keep 24 snapshots (24 hours)
- Manual snapshots never expire
- Auto-rollback on critical failures (optional)

### aios-config

**Override Hierarchy**:
```
1. Default values (compiled in)
2. /etc/aios/config.toml (system)
3. ~/.config/aios/config.toml (user)
4. Environment variables (AIOS_*)
5. Command-line flags
```

**Hot Reload** (future):
```rust
// Watch config file for changes
let watcher = ConfigWatcher::new(config_path);
watcher.on_change(|new_config| {
    daemon.reload(new_config)?;
});
```

## Security Model

### Sandbox Modes

**Safe Mode**:
- All commands require confirmation
- No sudo/systemctl/destructive operations
- Ideal for production

**Normal Mode** (default):
- Dangerous patterns blocked
- Critical operations require confirmation
- Balance of safety and usability

**Paranoid Mode**:
- Whitelist-only commands
- Every operation confirmed
- Maximum security

**Trusted Mode**:
- No restrictions
- For development/testing only

### Command Filtering

```toml
[security]
mode = "normal"

dangerous_patterns = [
    "rm -rf /",
    "mkfs.*",
    "dd if=/dev/zero",
]

require_confirm = [
    "sudo.*",
    "systemctl.*",
    "pacman -R.*",
]
```

## Container Integration

### Shared Memory Design

```
Host:
  ~/.config/aios/memory.db  (bind mount)

Container 1:
  ~/.config/aios/memory.db  (same file)

Container 2:
  ~/.config/aios/memory.db  (same file)
```

All containers share the same memory, enabling:
- Knowledge transfer between containers
- Consistent AI behavior
- Centralized audit log

### Recovery Across Containers

```bash
# In container 1
aios snapshot --description "Container 1 state"

# In container 2 (sees same snapshots)
aios snapshots
# Shows snapshots from all containers

aios restore <container-1-snapshot>
# Can restore from any container's snapshot
```

## Performance Considerations

### Unix Socket vs HTTP

```
Unix Socket:  ~10μs latency
HTTP:         ~1ms latency (100x slower)
```

AIOS uses Unix sockets for:
- Local daemon communication
- Low overhead
- No network stack
- Filesystem permissions

### Memory Database

```
SQLite:       In-process, no network
Vector DB:    Separate service, network overhead
```

For AIOS scale (thousands of memories), SQLite + FTS is:
- Faster (no RPC)
- Simpler (no separate service)
- Reliable (ACID guarantees)

### Async Everything

```rust
#[tokio::main]
async fn main() {
    // All I/O is async
    let memory = MemoryStore::new().await?;
    let response = llm.chat(messages).await?;
    let result = tool.execute(args).await?;
}
```

Benefits:
- Handle multiple clients
- Non-blocking I/O
- Efficient resource usage

## Future Enhancements

### Vector Memory

```rust
pub struct VectorMemory {
    embeddings: VectorDB,
    metadata: SQLite,
}

impl VectorMemory {
    async fn semantic_search(&self, query: &str)
        -> Vec<Memory> {
        let embedding = self.embed(query).await?;
        self.embeddings.search(embedding, 10).await?
    }
}
```

### Multi-Agent Coordination

```rust
pub struct Coordinator {
    agents: Vec<Box<dyn Agent>>,
}

impl Coordinator {
    async fn delegate(&self, task: &Task)
        -> Vec<Response> {
        // Find best agents for task
        let agents = self.find_experts(task);

        // Execute in parallel
        let futures: Vec<_> = agents.iter()
            .map(|a| a.handle(task))
            .collect();

        join_all(futures).await
    }
}
```

### Immutable OS Integration

```rust
pub struct ImageBuilder {
    base: OstreeRef,
}

impl ImageBuilder {
    async fn build_from_config(&self, config: &AIOSConfig)
        -> OSImage {
        // Generate image definition from config
        let definition = self.generate_definition(config);

        // Build image
        let image = self.build(definition).await?;

        // Test in sandbox
        self.test(image).await?;

        image
    }
}
```

## Conclusion

AIOS represents a fundamental rethinking of AI-powered OS management:

**Old**: Fragmented tools, manual coordination, limited recovery
**New**: Unified daemon, automatic coordination, built-in recovery

The design prioritizes:
1. **Simplicity**: One daemon, one config, one memory
2. **Safety**: Snapshots, rollback, security modes
3. **Flexibility**: Plugins, multi-provider, declarative config
4. **Performance**: Unix sockets, async I/O, SQLite

This foundation enables future innovations in AI-powered system management while maintaining a clean, maintainable architecture.
