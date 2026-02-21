#!/usr/bin/env bash
set -euo pipefail

# ── Konfiguration ──────────────────────────────────────────
REMOTE_USER="${DEPLOY_USER:-jakob}"
SERVICE_NAME="vaultagent"
TARGET="aarch64-unknown-linux-musl"

# Remote-Host abfragen (oder aus Argument / Umgebungsvariable)
if [ -n "${1:-}" ]; then
    REMOTE_IP="$1"
elif [ -n "${DEPLOY_HOST:-}" ]; then
    REMOTE_IP="$DEPLOY_HOST"
else
    read -rp "🖥  IP-Adresse / Hostname des Zielservers: " REMOTE_IP
fi

REMOTE_HOST="$REMOTE_USER@$REMOTE_IP"
REMOTE_DIR="/home/$REMOTE_USER/$SERVICE_NAME"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$SCRIPT_DIR/vaultagent"
BINARY="$PROJECT_DIR/target/$TARGET/release/$SERVICE_NAME"

# ── Cross-compile ─────────────────────────────────────────
echo "🔨 Baue für $TARGET …"
export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
export AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc

cd "$PROJECT_DIR"
cargo build --release --target "$TARGET"

BINARY_SIZE=$(du -h "$BINARY" | cut -f1)
echo "✅ Binary fertig ($BINARY_SIZE)"

# ── SSH-Verbindung einmalig aufbauen ──────────────────────
SOCKET="/tmp/deploy-ssh-$$"
cleanup() { ssh -o ControlPath="$SOCKET" -O exit "$REMOTE_HOST" 2>/dev/null || true; }
trap cleanup EXIT

echo "🔑 Verbinde mit $REMOTE_HOST (einmalige Passwort-Eingabe) …"
ssh -o ControlMaster=yes -o ControlPath="$SOCKET" -o ControlPersist=yes -fN "$REMOTE_HOST"

export SSH_OPTS="-o ControlPath=$SOCKET"
ssh()  { command ssh  $SSH_OPTS "$@"; }
scp()  { command scp  $SSH_OPTS "$@"; }

# ── Remote vorbereiten ────────────────────────────────────
echo "📦 Deploye nach $REMOTE_HOST:$REMOTE_DIR …"

# Service stoppen (falls vorhanden)
ssh "$REMOTE_HOST" "sudo systemctl stop $SERVICE_NAME 2>/dev/null || true"

# Verzeichnisstruktur erstellen
ssh "$REMOTE_HOST" "mkdir -p $REMOTE_DIR/soul/memory $REMOTE_DIR/skills $REMOTE_DIR/cron"

# ── Dateien kopieren ──────────────────────────────────────
# Binary
scp "$BINARY" "$REMOTE_HOST:$REMOTE_DIR/$SERVICE_NAME"

# .env (Secrets + Konfiguration — wird von systemd als EnvironmentFile geladen)
scp "$PROJECT_DIR/.env" "$REMOTE_HOST:$REMOTE_DIR/.env"

# Trusted Chat-IDs
if [ -f "$PROJECT_DIR/trusted_chat_ids.md" ]; then
    scp "$PROJECT_DIR/trusted_chat_ids.md" "$REMOTE_HOST:$REMOTE_DIR/trusted_chat_ids.md"
fi

# Soul (Persönlichkeit + Gedächtnis)
scp -r "$PROJECT_DIR/soul/" "$REMOTE_HOST:$REMOTE_DIR/soul/"

# Python-Skills
scp -r "$PROJECT_DIR/skills/" "$REMOTE_HOST:$REMOTE_DIR/skills/"

# Cron-Jobs (falls vorhanden)
if [ -f "$PROJECT_DIR/cron/jobs.json" ]; then
    scp "$PROJECT_DIR/cron/jobs.json" "$REMOTE_HOST:$REMOTE_DIR/cron/jobs.json"
fi

# ── Systemd Service einrichten ────────────────────────────
echo "⚙️  Richte systemd-Service ein …"

ssh "$REMOTE_HOST" "chmod +x $REMOTE_DIR/$SERVICE_NAME"

# Service-Unit auf den Pi schreiben
ssh "$REMOTE_HOST" "sudo tee /etc/systemd/system/$SERVICE_NAME.service > /dev/null" <<EOF
[Unit]
Description=VaultAgent – Personal AI Assistant
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$REMOTE_USER
WorkingDirectory=$REMOTE_DIR
ExecStart=$REMOTE_DIR/$SERVICE_NAME
EnvironmentFile=$REMOTE_DIR/.env
Restart=always
RestartSec=5

# Logging via journald
StandardOutput=journal
StandardError=journal
SyslogIdentifier=$SERVICE_NAME

[Install]
WantedBy=multi-user.target
EOF

ssh "$REMOTE_HOST" "sudo systemctl daemon-reload && sudo systemctl enable $SERVICE_NAME"

# ── Starten ───────────────────────────────────────────────
echo "🚀 Starte $SERVICE_NAME …"
ssh "$REMOTE_HOST" "sudo systemctl start $SERVICE_NAME"

# Kurz warten und Status prüfen
sleep 2
ssh "$REMOTE_HOST" "systemctl is-active $SERVICE_NAME" && echo "✅ Service läuft!" || echo "❌ Service nicht gestartet — check logs"

echo ""
echo "   Logs:    ssh $REMOTE_HOST 'journalctl -u $SERVICE_NAME -f'"
echo "   Status:  ssh $REMOTE_HOST 'systemctl status $SERVICE_NAME'"
echo "   Stop:    ssh $REMOTE_HOST 'sudo systemctl stop $SERVICE_NAME'"
echo "   Restart: ssh $REMOTE_HOST 'sudo systemctl restart $SERVICE_NAME'"
cd ..
