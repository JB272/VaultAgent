#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$ROOT_DIR/vaultagent"
SECURE_ENV="$PROJECT_DIR/.env.secure"
DOCKER_ENV="$PROJECT_DIR/.env.docker"
TRUSTED_IDS="$PROJECT_DIR/trusted_chat_ids.md"

ensure_file() {
    local src="$1"
    local dst="$2"
    if [ ! -f "$dst" ]; then
        cp "$src" "$dst"
        echo "Created $dst"
    fi
}

set_kv() {
    local file="$1"
    local key="$2"
    local value="$3"
    if grep -qE "^${key}=" "$file"; then
        # macOS sed requires an empty extension for -i
        sed -i '' "s|^${key}=.*|${key}=${value}|" "$file"
    else
        echo "${key}=${value}" >> "$file"
    fi
}

prompt() {
    local label="$1"
    local default="${2:-}"
    local value
    if [ -n "$default" ]; then
        read -rp "$label [$default]: " value
        echo "${value:-$default}"
    else
        read -rp "$label: " value
        echo "$value"
    fi
}

ensure_file "$PROJECT_DIR/.env.secure.example" "$SECURE_ENV"
ensure_file "$PROJECT_DIR/.env.docker.example" "$DOCKER_ENV"

TELEGRAM_BOT_TOKEN="$(prompt "Telegram bot token")"
while [ -z "$TELEGRAM_BOT_TOKEN" ]; do
    TELEGRAM_BOT_TOKEN="$(prompt "Telegram bot token (required)")"
done

LLM_PROVIDER="$(prompt "LLM provider (openai|anthropic)" "openai")"
if [ "$LLM_PROVIDER" != "openai" ] && [ "$LLM_PROVIDER" != "anthropic" ]; then
    echo "Invalid provider. Using openai."
    LLM_PROVIDER="openai"
fi

if [ "$LLM_PROVIDER" = "openai" ]; then
    OPENAI_API_KEY="$(prompt "OpenAI API key")"
    while [ -z "$OPENAI_API_KEY" ]; do
        OPENAI_API_KEY="$(prompt "OpenAI API key (required)")"
    done
    OPENAI_BASE_URL="$(prompt "OpenAI base URL" "https://api.openai.com/v1")"
    OPENAI_MODEL="$(prompt "OpenAI model" "gpt-4o-mini")"
fi

if [ "$LLM_PROVIDER" = "anthropic" ]; then
    ANTHROPIC_API_KEY="$(prompt "Anthropic API key")"
    while [ -z "$ANTHROPIC_API_KEY" ]; do
        ANTHROPIC_API_KEY="$(prompt "Anthropic API key (required)")"
    done
    ANTHROPIC_MODEL="$(prompt "Anthropic model" "claude-3-5-sonnet-latest")"
fi

WORKER_TOKEN="$(prompt "Worker token (shared secret)")"
while [ -z "$WORKER_TOKEN" ]; do
    WORKER_TOKEN="$(prompt "Worker token (required)")"
done

TIMEZONE="$(prompt "Timezone" "Europe/Berlin")"

CHAT_IDS="$(prompt "Telegram chat IDs (comma-separated, blank = allow all)" "")"

# Write .env.secure values
set_kv "$SECURE_ENV" "TELEGRAM_BOT_TOKEN" "$TELEGRAM_BOT_TOKEN"
set_kv "$SECURE_ENV" "LLM_PROVIDER" "$LLM_PROVIDER"
set_kv "$SECURE_ENV" "TIMEZONE" "$TIMEZONE"
set_kv "$SECURE_ENV" "WORKER_TOKEN" "$WORKER_TOKEN"

if [ "$LLM_PROVIDER" = "openai" ]; then
    set_kv "$SECURE_ENV" "OPENAI_API_KEY" "$OPENAI_API_KEY"
    set_kv "$SECURE_ENV" "OPENAI_BASE_URL" "$OPENAI_BASE_URL"
    set_kv "$SECURE_ENV" "OPENAI_MODEL" "$OPENAI_MODEL"
fi

if [ "$LLM_PROVIDER" = "anthropic" ]; then
    set_kv "$SECURE_ENV" "ANTHROPIC_API_KEY" "$ANTHROPIC_API_KEY"
    set_kv "$SECURE_ENV" "ANTHROPIC_MODEL" "$ANTHROPIC_MODEL"
fi

if [ -n "$CHAT_IDS" ]; then
    set_kv "$SECURE_ENV" "TELEGRAM_ALLOWED_CHAT_IDS" "$CHAT_IDS"
fi

# Keep worker token in sync
set_kv "$DOCKER_ENV" "WORKER_TOKEN" "$WORKER_TOKEN"

# Write trusted_chat_ids.md if user provided IDs
if [ -n "$CHAT_IDS" ]; then
    {
        echo "# Trusted Chat IDs"
        echo ""
        echo "<!-- Telegram Chat-IDs, eine pro Zeile. Zeilen mit # werden ignoriert. -->"
        echo ""
        echo "$CHAT_IDS" | tr ',' '\n' | sed 's/^ *//; s/ *$//'
    } > "$TRUSTED_IDS"
    echo "Updated $TRUSTED_IDS"
fi

echo ""
echo "Setup complete. Next steps (locally):"
echo "1) cd vaultagent"
echo "2) docker compose up -d"
echo "3) export \$(grep -v '^#' .env.secure | xargs)"
echo "4) cargo run"
echo "Or (recommended) remote:"
echo "./deploy.sh <ipaddress>"