# agents.md --- VaultAgent (Rust Agent Runtime)

> VaultAgent ist eine sichere, kostenkontrollierte Agent-Runtime in Rust.
> Unterstützt API-LLMs und lokale LLMs über eine einheitliche Provider-Schnittstelle.
> Fokus: Security, Determinismus, Kostenkontrolle.

---

# 1. Ziel & Prinzipien

## 1.1 Ziel

VaultAgent soll:

- LLMs (API oder lokal) einheitlich ansprechen
- Tools kontrolliert ausführen
- harte Sicherheits- und Budgetregeln erzwingen
- jeden Run vollständig auditierbar machen

## 1.2 Leitprinzipien

- Security First
- Local First
- Fail Closed
- Deny-by-Default
- Deterministische Logs
- Kostenkontrolle

---

# 2. Architektur

## 2.1 Logische Gesamtarchitektur

CLI / API  
↓  
Core Runner (Agent Loop, State, Routing, Budget)  
↓  
Provider Layer | Tool Engine | Policies  
↓  
Audit Logger (`runs/<run_id>/`)

---

## 2.2 Core Runtime

Verantwortlich für:

- Agent Loop (Plan → Tool → Observe → Decide → Final)
- State Management
- Policy Enforcement
- Budget Tracking
- Audit Logging

Ablauf:

1. Prompt-Kontext aufbauen
2. Modell aufrufen
3. Tool Calls validieren & ausführen
4. Ergebnisse anhängen
5. Wiederholen bis Final oder Limit erreicht

---

## 2.3 Agent Loop Detail

User Task  
↓  
Build Prompt Context  
↓  
Call Model (small)  
├── Final → Done  
└── Tool Calls → Validate → Execute → Append → Budget Check → Loop

Optional:  
NEED_UPGRADE → Switch Model → Retry Step

---

## 2.4 Provider Layer

Alle Provider implementieren:

- `chat_complete(request) -> response`
- optional streaming
- optional embeddings

Unterstützte Modi:

- API-Provider
- Lokale OpenAI-kompatible Endpoints (vLLM, llama.cpp server, Ollama, LM Studio)

Interne Kommunikation erfolgt OpenAI-kompatibel.

---

## 2.5 Tool Layer

Eigenschaften:

- JSON Input
- JSON Output
- Schema-validierung
- Explizite Aktivierung
- Per Run konfigurierbar

Standard: Kein Tool aktiv.

---

# 3. Sicherheitsrichtlinien

## 3.1 Harte Limits

Konfigurierbar:

- max_steps
- max_tool_calls
- max_output_tokens
- max_total_tokens
- max_retries_per_step
- step_timeout_seconds

Bei Limitüberschreitung → Abbruch.

---

## 3.2 Filesystem-Regeln

- Zugriff nur innerhalb Workspace-Root
- Kein Zugriff auf Systempfade
- Max Dateigröße begrenzt

---

## 3.3 Netzwerk-Regeln

- Standard: deaktiviert
- Nur Allowlist-Domains
- Timeout verpflichtend
- Max Response-Größe

---

## 3.4 Shell-Regeln

Standard: deaktiviert.

Nur erlaubt wenn:

- explizit aktiviert
- sandboxed
- Ressourcenlimitiert
- kein freier Netzwerkzugriff

---

## 3.5 Secret Redaction

Logs dürfen keine Secrets enthalten.
Zu maskieren:

- Authorization Header
- API Tokens
- Umgebungsvariablen
- Hochentropische Strings

---

# 4. Model Routing

## 4.1 Default

- Lokales kleines Modell

## 4.2 Upgrade

Upgrade auf großes lokales oder API-Modell wenn:

- Nutzer es verlangt
- kleines Modell scheitert
- Komplexität hoch
- Modell meldet NEED_UPGRADE

## 4.3 Confidence-Protokoll

Modelle dürfen liefern:

- OK(confidence=0.0..1.0)
- NEED_UPGRADE(reason=...)

---

# 5. Audit Logging

Jeder Run erzeugt: `runs/<run_id>/`

Enthält:

`events.jsonl`

- model_request (redacted)
- model_response (redacted)
- tool_call
- tool_result
- routing_decision
- policy_violation

`summary.json`

- Gesamt-Tokens
- Tool-Anzahl
- Modellwechsel
- Fehler
- Finales Ergebnis

---

# 6. Projektstruktur (Rust Workspace)

```text
vaultagent/
├── Cargo.toml
├── agents.md
├── configs/
│   └── default.toml
├── runs/
└── crates/
    ├── core/
    ├── providers/
    ├── tools/
    ├── policy/
    ├── audit/
    ├── cli/
    └── server/ (optional)
```

---

# 7. Built-in Tools (MVP)

## Filesystem

- fs.read_file
- fs.write_file
- fs.list_dir

## HTTP

- http.get
- http.post

## Shell

- shell.exec (nur explizit + sandboxed)

---

# 8. CLI Beispiele

```bash
vaultagent run --task "Datei zusammenfassen"
vaultagent run --provider local
vaultagent run --enable-tool fs.read_file
vaultagent run --allow-net api.example.com
vaultagent run --enable-shell
```

---

# 9. Betriebsregeln

- Lokal vor API
- Klein vor Groß
- Kein Tool ohne explizite Aktivierung
- Kein Netzwerk ohne Allowlist
- Kein Shell ohne Sandbox
- Jeder Run wird geloggt
- Policy-Verletzung → Abbruch

---

VaultAgent ist eine sichere, deterministische, kostenkontrollierte Agent-Runtime mit klarer Trennung von LLM-Provider, Tool-Ausführung und Policy Enforcement.
