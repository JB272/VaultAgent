# AGENTS.md

This file defines how coding agents should work in this repository.

## Scope

- Applies to the whole repository unless a deeper `AGENTS.md` overrides it.
- Main target: stable, execution-first behavior for VaultAgent.

## Project Structure

- `vaultagent/` - Rust application (host orchestrator + worker mode).
- `data/container/` - runtime seed data copied to target (`soul/`, `skills/`, `cron/`).
- `data/host/` - host-only files (for example constitution).
- `setup.sh` - creates/updates `.env` files.
- `deploy.sh` - build + deploy to Raspberry Pi/remote host.

## Runtime Model

- Host process handles Telegram/Web, LLM calls, orchestration.
- Worker process runs tools in Docker (`--worker` mode).
- Uploaded Telegram files are persisted in workspace paths and must remain retrievable.

## Commands

- Format/check:
  - `cd vaultagent && cargo fmt`
  - `cd vaultagent && cargo check`
- Run locally:
  - `cd vaultagent && docker compose up -d`
  - `cd vaultagent && export $(grep -v '^#' .env.secure | xargs) && cargo run`
- Deploy:
  - `./deploy.sh jarvis`

## Engineering Rules

1. Prefer concrete execution over high-level suggestions.
2. Keep changes backward-compatible unless explicitly requested.
3. When changing upload/memory behavior, preserve existing paths or provide migration notes.
4. Do not hardcode secrets or commit `.env` values.
5. For user-facing behavior changes, include clear fallback behavior.
6. For queue/concurrency changes, avoid message loss and preserve chat ordering.
7. Default language is english but we want to make it multilingual.
8. No errorhandling based on Strings in LLM Answeres since we do not know the language.

## Definition of Done

- Code compiles (`cargo check`) after changes.
- No obvious regression in Telegram upload flow.
- New behavior is discoverable by the model via prompt/tool descriptions.
