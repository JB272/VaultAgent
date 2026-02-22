# VaultAgent

A personal AI assistant written in Rust, heavily inspired by [OpenClaw](https://github.com/openclaw/openclaw). We loved the concept of OpenClaw as a self-hostable, tool-using AI agent with a persistent soul and memory, and wanted to rebuild it from scratch in Rust for performance, low resource usage, and the joy of systems programming.

VaultAgent runs on a Raspberry Pi (or any Linux server), connects to Telegram, and acts as your personal assistant with long-term memory, scheduled tasks, voice transcription, and extensible skills.

## Motivation

I loved using [OpenClaw](https://github.com/openclaw/openclaw), but I was always afraid to leave it running overnight. What if a bug caused it to loop and rack up massive API costs? What if my API keys got leaked? The thought of waking up to a surprise bill kept me from truly trusting the setup.

VaultAgent is my attempt to rebuild the same idea as a personal, self-hosted AI agent with a soul and memory, but with safety and control as a first-class priority. Written in Rust, it compiles to a single static binary, runs on minimal resources, and gives me the confidence to let it run 24/7 without worrying about runaway costs or exposed credentials.

## Features

### Working

- **Sandboxed tool execution**: All skills/tools run inside a Docker container — LLM keys and Telegram tokens never enter the sandbox
- **Telegram Bot**: Polling mode (no public URL needed) and webhook mode
- **LLM Integration**: OpenAI-compatible API (GPT-4o-mini, or any compatible provider)
- **Tool/Skill System**: The agent can call tools during conversations:
  - `read_file` / `write_file` / `list_directory`: File system access within the sandbox workspace
  - `web_search` / `web_fetch` / `research`: Search the web, fetch pages, or do deep research via subagent
  - `memory_save` / `memory_search`: Persistent long-term memory (Markdown files)
  - `cron_add` / `cron_list` / `cron_remove`: Schedule reminders and recurring tasks
  - **Python skills**: Drop a `.py` script into `skills/` and it's auto-loaded as a tool
- **Voice Messages**: Telegram voice memos are transcribed via Whisper and processed as text
- **Cron Scheduler**: Schedule one-shot or recurring tasks ("remind me at 19:30 to close the window")
- **Soul**: Personality and memory defined in Markdown files (`soul/personality.md`, `soul/memory/`)
- **Timezone-aware**: Converts user-local times to UTC for scheduling
- **Chat ID allowlist**: Only trusted Telegram users can interact with the bot
- **Telegram commands**: Built-in slash commands for runtime control (see below)
- **Deploy script**: One-command cross-compile and deploy to a Raspberry Pi via SSH + systemd + Docker
- **Web Chat**: Basic browser-based chat interface (localhost)

### Not Yet Implemented

- Persistent chat history (currently in-memory only, lost on restart)
- Streaming responses
- Multi-user support (separate conversation histories per chat)
- Image understanding / vision
- Database backend (currently file-based)
- Rate limiting

## Architecture

VaultAgent uses a **split-process security model**: the host orchestrator handles Telegram, LLM calls, and secrets, while all tool/skill execution runs inside a sandboxed Docker container.

```
┌─────────────────────────────────────────────────┐
│  HOST (Raspberry Pi / Server)               │
│                                              │
│  .env.secure (API keys, tokens)              │
│                                              │
│  ┌─────────────┐  ┌─────────────┐       │
│  │  Telegram  │  │  Web Chat   │       │
│  │  Gateway   │  │  Gateway   │       │
│  └──────┬──────┘  └──────┬──────┘       │
│       │              │                │
│       └──────┬───────┘                │
│              ▼                         │
│      ┌───────────────┐  ┌──────────┐  │
│      │    Agent      │  │   Soul   │  │
│      │  (LLM loop)   │◄─┤(readonly)│  │
│      └───────┬───────┘  └──────────┘  │
│              │ HTTP (:9100)            │
└──────────────┼───────────────────────┘
               │
┌──────────────┼───────────────────────┐
│  DOCKER SANDBOX                          │
│  .env.docker (no secrets!)               │
│                                          │
│  ┌──────────────────────────────────┐  │
│  │  Worker HTTP API (:9100)          │  │
│  │  POST /execute  → run skills     │  │
│  │  GET  /definitions               │  │
│  └──────────────────────────────────┘  │
│                                          │
│  Mounted: soul/, skills/, cron/          │
│  Security: read_only rootfs,             │
│    no-new-privileges, cap_drop ALL,      │
│    512 MB RAM, 100 PIDs                  │
└────────────────────────────────────────┘
```

**Key security properties:**

- API keys (`LLM_API_KEY`, `TELEGRAM_BOT_TOKEN`) exist only on the host — never in the container
- The agent (LLM) cannot see its own source code, binary, or environment variables
- Container runs with read-only root filesystem, no capabilities, no privilege escalation
- Resource-limited: 512 MB RAM, 100 PIDs
- Worker API is authenticated with `WORKER_TOKEN`
- Mounted directories (`soul/`, `skills/`, `cron/`) are the only writable paths

## Getting Started

### Prerequisites

- **Rust** (edition 2024): [Install](https://rustup.rs/)
- **Docker** (with Docker Compose): Required on the deployment server for the sandbox worker
- **Telegram Bot Token**: Create one via [@BotFather](https://t.me/BotFather)
- **OpenAI API Key** (or any OpenAI-compatible provider)
- **For deployment**: A Linux aarch64 server (e.g. Raspberry Pi 3/4/5 with 64-bit OS)

### Setup

1. **Clone the repository**

   ```bash
   git clone https://github.com/your-username/vaultagent.git
   cd vaultagent
   ```

2. **Create your environment files**

   ```bash
   cp vaultagent/.env.secure.example vaultagent/.env.secure
   cp vaultagent/.env.docker.example vaultagent/.env.docker
   ```

   - `.env.secure` — **host-only**, contains `TELEGRAM_BOT_TOKEN`, `LLM_API_KEY`, `WORKER_TOKEN`
   - `.env.docker` — **sandbox-only**, contains `WORKER_TOKEN` (must match) and non-secret config

   **Important:** Use the same `WORKER_TOKEN` value in both files.

3. **Set your trusted Telegram chat IDs**

   Edit `vaultagent/trusted_chat_ids.md` and add your Telegram chat ID (one per line).  
   Don't know your ID? Send a message to the bot — unauthorized users get a reply with their chat ID.

4. **Customize the personality** (optional)

   Edit `vaultagent/soul/personality.md` to change how the agent behaves.

5. **Run locally (development)**

   First start the sandbox worker:

   ```bash
   cd vaultagent
   docker compose up -d
   ```

   Then run the host orchestrator:

   ```bash
   # Source .env.secure for host secrets
   export $(grep -v '^#' .env.secure | xargs)
   cargo run
   ```

### Binary Size

Current local build sizes:

- **Debug** (`target/debug/vaultagent`): **20M** (`21,255,448` bytes)
- **Release** (`target/release/vaultagent`): **6.3M** (`6,583,280` bytes)

### Deploy to a Raspberry Pi

The included `deploy.sh` cross-compiles for `aarch64-unknown-linux-musl` (statically linked, no dependencies), copies everything to your server, and sets up a systemd service.

**First-time setup:**

```bash
# Install the cross-compilation target
rustup target add aarch64-unknown-linux-musl

# You'll also need a cross-linker (e.g. via Homebrew on macOS)
brew install filosottile/musl-cross/musl-cross --with-aarch64
```

**Deploy:**

```bash
./deploy.sh                   # prompts for server IP
./deploy.sh 192.168.1.42      # or pass it directly
DEPLOY_HOST=jarvis ./deploy.sh  # or via env var
```

The script will:

- Cross-compile a release binary
- SSH into the server (one password prompt, reused for all operations)
- Copy the binary, env files, soul, skills, cron jobs, Docker files, and trusted chat IDs
- Build and start the sandbox Docker container
- Set up and start a systemd service (`vaultagent.service`)

**Useful commands after deploy:**

```bash
ssh user@server 'journalctl -u vaultagent -f'        # view logs
ssh user@server 'sudo systemctl restart vaultagent'   # restart
ssh user@server 'sudo systemctl stop vaultagent'      # stop
```

Or just send `/reboot` in Telegram.

## Telegram Commands

The bot responds to these slash commands directly, without involving the LLM:

| Command          | Description                                                               |
| ---------------- | ------------------------------------------------------------------------- |
| `/tools`         | List all available skills/tools registered in this instance               |
| `/stats`         | Show today's LLM token usage (requests, prompt tokens, completion tokens) |
| `/models`        | Show the currently active LLM model                                       |
| `/models <name>` | Switch to a different model at runtime (e.g. `/models gpt-4o`)            |
| `/reboot`        | Restart the service (systemd will bring it back up automatically)         |

## Project Structure

```
vaultagent/
├── src/
│   ├── main.rs                  # Entry point, event loop, --worker mode
│   ├── worker.rs                # Sandbox worker HTTP server
│   ├── gateway/                 # Communication channels
│   │   ├── IncomingActionsQueue.rs
│   │   └── com/
│   │       ├── telegram/        # Telegram bot (polling + webhook)
│   │       └── website/         # Web chat interface
│   ├── reasoning/               # LLM integration
│   │   ├── agent.rs             # Agent orchestration (tool loop)
│   │   ├── llm_interface.rs     # LLM abstraction
│   │   ├── llmApis/openAI.rs    # OpenAI-compatible client
│   │   ├── usage.rs             # Token usage tracking
│   │   └── transcription.rs     # Whisper voice transcription
│   ├── skills/                  # Tool/skill system
│   │   ├── mod.rs               # SkillRegistry + RemoteSkillProxy
│   │   ├── default_skills/      # Built-in skills (Rust)
│   │   └── python_skill.rs      # Auto-loaded Python skills
│   ├── cron/                    # Scheduled tasks
│   │   ├── store.rs             # Job persistence (JSON)
│   │   └── scheduler.rs         # Background scheduler
│   └── soul/                    # Personality + memory loaders
├── soul/                        # Soul data files
│   ├── personality.md
│   └── memory/
├── skills/                      # Python skill scripts
├── cron/                        # Cron job storage
├── trusted_chat_ids.md          # Telegram allowlist
├── .env.secure.example          # Host secrets template
├── .env.docker.example          # Sandbox env template
├── Dockerfile.worker            # Sandbox container image
├── docker-compose.yml           # Sandbox orchestration
└── Cargo.toml
```

## License

This project is licensed under the **GNU Affero General Public License v3.0 (AGPL-3.0)**. See the [LICENSE](LICENSE) file for details.

You are free to use, modify, and distribute this software, as long as any modified versions (including those running as a network service) remain open source under the same license.

## Acknowledgments

This project is inspired by [OpenClaw](https://github.com/openclaw/openclaw), an open-source AI agent framework. We share the same vision of a personal, self-hosted AI assistant with a soul, memory, and extensible skills, just rebuilt in Rust.
