# VaultAgent

Sichere, deterministische und kostenkontrollierte Agent-Runtime in Rust.

## Fokus
- Security First
- Local First
- Fail Closed / Deny-by-Default
- Auditierbarkeit pro Run
- Klare Budget- und Policy-Grenzen

## Struktur
- `agents.md` – Produkt- und Architekturkonzept
- `configs/default.toml` – Basis-Konfiguration
- `runs/` – Audit-Logs pro Run
- `crates/*` – modulare Runtime-Bausteine

## Start
```bash
cargo check
cargo run -p vaultagent-cli -- --help
```
