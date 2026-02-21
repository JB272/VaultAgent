# VaultAgent

A personal AI assistant written in Rust, heavily inspired by [OpenClaw](https://github.com/openclaw/openclaw). We loved the concept of OpenClaw — a self-hostable, tool-using AI agent with a persistent soul and memory — and wanted to rebuild it from scratch in Rust for performance, low resource usage, and the joy of systems programming.

VaultAgent runs on a Raspberry Pi (or any Linux server), connects to Telegram, and acts as your personal assistant with long-term memory, scheduled tasks, voice transcription, and extensible skills.

## Features

### Working

- **Telegram Bot** — Polling mode (no public URL needed) and webhook mode
- **LLM Integration** — OpenAI-compatible API (GPT-4o-mini, or any compatible provider)
- **Tool/Skill System** — The agent can call tools during conversations:
  - `read_file` / `write_file` / `list_directory` — File system access within the workspace
  - `web_search` — Search the web or fetch URLs
  - `memory_save` / `memory_search` — Persistent long-term memory (Markdown files)
  - `cron_add` / `cron_list` / `cron_remove` — Schedule reminders and recurring tasks
  - **Python skills** — Drop a `.py` script into `skills/` and it's auto-loaded as a tool
- **Voice Messages** — Telegram voice memos are transcribed via Whisper and processed as text
- **Cron Scheduler** — Schedule one-shot or recurring tasks ("remind me at 19:30 to close the window")
- **Soul** — Personality and memory defined in Markdown files (`soul/personality.md`, `soul/memory/`)
- **Timezone-aware** — Converts user-local times to UTC for scheduling
- **Chat ID allowlist** — Only trusted Telegram users can interact with the bot
- **`/reboot` command** — Restart the service remotely via Telegram
- **Deploy script** — One-command cross-compile and deploy to a Raspberry Pi via SSH + systemd
- **Web Chat** — Basic browser-based chat interface (localhost)

### Not Yet Implemented

- Persistent chat history (currently in-memory only — lost on restart)
- Streaming responses
- Multi-user support (separate conversation histories per chat)
- Image understanding / vision
- Database backend (currently file-based)
- Rate limiting

## Architecture

```
┌─────────────┐     ┌─────────────┐
│  Telegram   │     │  Web Chat   │
│  Gateway    │     │  Gateway    │
└──────┬──────┘     └──────┬──────┘
       │                   │
       └───────┬───────────┘
               ▼
       ┌───────────────┐
       │ IncomingAction │
       │    Queue       │
       └───────┬───────┘
               ▼
       ┌───────────────┐      ┌────────────┐
       │    Agent      │◄────►│   Skills   │
       │  (LLM loop)  │      │  Registry  │
       └───────┬───────┘      └────────────┘
               │
       ┌───────┴───────┐
       │     Soul      │
       │ (personality  │
       │  + memory)    │
       └───────────────┘
```

## Getting Started

### Prerequisites

- **Rust** (edition 2024) — [Install](https://rustup.rs/)
- **Telegram Bot Token** — Create one via [@BotFather](https://t.me/BotFather)
- **OpenAI API Key** (or any OpenAI-compatible provider)
- **For deployment:** A Linux aarch64 server (e.g. Raspberry Pi 3/4/5 with 64-bit OS)

### Setup

1. **Clone the repository**

   ```bash
   git clone https://github.com/your-username/vaultagent.git
   cd vaultagent
   ```

2. **Create your `.env` file**

   ```bash
   cp vaultagent/.env_example vaultagent/.env
   ```

   Fill in your `TELEGRAM_BOT_TOKEN` and `LLM_API_KEY`.

3. **Set your trusted Telegram chat IDs**

   Edit `vaultagent/trusted_chat_ids.md` and add your Telegram chat ID (one per line).  
   Don't know your ID? Send a message to the bot — unauthorized users get a reply with their chat ID.

4. **Customize the personality** (optional)

   Edit `vaultagent/soul/personality.md` to change how the agent behaves.

5. **Run locally**
   ```bash
   cd vaultagent
   cargo run
   ```

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
- Copy the binary, `.env`, soul, skills, cron jobs, and trusted chat IDs
- Set up and start a systemd service (`vaultagent.service`)

**Useful commands after deploy:**

```bash
ssh user@server 'journalctl -u vaultagent -f'        # view logs
ssh user@server 'sudo systemctl restart vaultagent'   # restart
ssh user@server 'sudo systemctl stop vaultagent'      # stop
```

Or just send `/reboot` in Telegram.

## Project Structure

```
vaultagent/
├── src/
│   ├── main.rs                  # Entry point, event loop
│   ├── gateway/                 # Communication channels
│   │   ├── IncomingActionsQueue.rs
│   │   └── com/
│   │       ├── telegram/        # Telegram bot (polling + webhook)
│   │       └── website/         # Web chat interface
│   ├── reasoning/               # LLM integration
│   │   ├── agent.rs             # Agent orchestration (tool loop)
│   │   ├── llm_interface.rs     # LLM abstraction
│   │   ├── llmApis/openAI.rs    # OpenAI-compatible client
│   │   └── transcription.rs     # Whisper voice transcription
│   ├── skills/                  # Tool/skill system
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
├── .env_example                 # Environment template
└── Cargo.toml
```

## Acknowledgments

This project is inspired by [OpenClaw](https://github.com/openclaw/openclaw), an open-source AI agent framework. We share the same vision of a personal, self-hosted AI assistant with a soul, memory, and extensible skills — just rebuilt in Rust.
