use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::reasoning::llm_interface::LlmToolDefinition;
use crate::skills::Skill;

pub struct EmailMailboxSkill;

const MAILBOX_BRIDGE: &str = r#"
import email
import imaplib
import json
import os
import poplib
import smtplib
import ssl
import sys
from datetime import datetime, timezone
from email.header import decode_header
from email.message import EmailMessage


def decode_mime_header(value):
    if not value:
        return ""
    out = []
    for part, enc in decode_header(value):
        if isinstance(part, bytes):
            out.append(part.decode(enc or "utf-8", errors="replace"))
        else:
            out.append(part)
    return "".join(out)


def extract_plain_text(msg, max_chars=4000):
    if msg.is_multipart():
        for part in msg.walk():
            content_type = part.get_content_type()
            disposition = str(part.get("Content-Disposition", "")).lower()
            if content_type == "text/plain" and "attachment" not in disposition:
                payload = part.get_payload(decode=True) or b""
                charset = part.get_content_charset() or "utf-8"
                return payload.decode(charset, errors="replace")[:max_chars]
        return ""
    payload = msg.get_payload(decode=True) or b""
    charset = msg.get_content_charset() or "utf-8"
    return payload.decode(charset, errors="replace")[:max_chars]


def env_config():
    cfg = {
        "email": os.getenv("MAILBOX_EMAIL_ADDRESS", "").strip(),
        "password": os.getenv("MAILBOX_EMAIL_PASSWORD", "").strip(),
        "imap_host": os.getenv("MAILBOX_IMAP_HOST", "").strip(),
        "imap_port": int(os.getenv("MAILBOX_IMAP_PORT", "993")),
        "pop3_host": os.getenv("MAILBOX_POP3_HOST", "").strip(),
        "pop3_port": int(os.getenv("MAILBOX_POP3_PORT", "995")),
        "smtp_host": os.getenv("MAILBOX_SMTP_HOST", "").strip(),
        "smtp_port": int(os.getenv("MAILBOX_SMTP_PORT", "587")),
    }
    if not cfg["email"] or not cfg["password"]:
        raise RuntimeError("Missing MAILBOX_EMAIL_ADDRESS or MAILBOX_EMAIL_PASSWORD")
    if not cfg["smtp_host"]:
        raise RuntimeError("Missing MAILBOX_SMTP_HOST")
    return cfg


def list_inbox_imap(arguments, cfg):
    if not cfg["imap_host"]:
        return {"ok": False, "error": "Missing MAILBOX_IMAP_HOST"}

    limit = int(arguments.get("limit", 10))
    limit = max(1, min(limit, 50))
    mailbox = arguments.get("mailbox", "INBOX")

    with imaplib.IMAP4_SSL(cfg["imap_host"], cfg["imap_port"]) as imap:
        imap.login(cfg["email"], cfg["password"])
        status, _ = imap.select(mailbox, readonly=True)
        if status != "OK":
            return {"ok": False, "error": f"Cannot open mailbox: {mailbox}"}

        status, data = imap.uid("search", None, "ALL")
        if status != "OK":
            return {"ok": False, "error": "IMAP search failed"}

        all_uids = data[0].split() if data and data[0] else []
        picked = all_uids[-limit:]

        mails = []
        for uid in reversed(picked):
            status, msg_data = imap.uid("fetch", uid, "(RFC822.HEADER)")
            if status != "OK" or not msg_data or not isinstance(msg_data[0], tuple):
                continue
            msg = email.message_from_bytes(msg_data[0][1])
            mails.append(
                {
                    "id": uid.decode("utf-8", errors="replace"),
                    "from": decode_mime_header(msg.get("From", "")),
                    "subject": decode_mime_header(msg.get("Subject", "")),
                    "date": msg.get("Date", ""),
                }
            )

        return {
            "ok": True,
            "protocol": "imap",
            "mailbox": mailbox,
            "count": len(mails),
            "emails": mails,
            "fetched_at_utc": datetime.now(timezone.utc).isoformat(),
        }


def list_inbox_pop3(arguments, cfg):
    if not cfg["pop3_host"]:
        return {"ok": False, "error": "Missing MAILBOX_POP3_HOST"}

    limit = int(arguments.get("limit", 10))
    limit = max(1, min(limit, 50))

    with poplib.POP3_SSL(cfg["pop3_host"], cfg["pop3_port"], timeout=30) as pop:
        pop.user(cfg["email"])
        pop.pass_(cfg["password"])

        _, items, _ = pop.list()
        total = len(items)
        if total == 0:
            return {
                "ok": True,
                "protocol": "pop3",
                "count": 0,
                "emails": [],
                "fetched_at_utc": datetime.now(timezone.utc).isoformat(),
            }

        first = max(1, total - limit + 1)
        ids = list(range(first, total + 1))

        mails = []
        for msg_id in reversed(ids):
            _, lines, _ = pop.top(msg_id, 0)
            header_bytes = b"\n".join(lines)
            msg = email.message_from_bytes(header_bytes)
            mails.append(
                {
                    "id": str(msg_id),
                    "from": decode_mime_header(msg.get("From", "")),
                    "subject": decode_mime_header(msg.get("Subject", "")),
                    "date": msg.get("Date", ""),
                }
            )

        return {
            "ok": True,
            "protocol": "pop3",
            "count": len(mails),
            "emails": mails,
            "fetched_at_utc": datetime.now(timezone.utc).isoformat(),
        }


def read_email_imap(arguments, cfg):
    if not cfg["imap_host"]:
        return {"ok": False, "error": "Missing MAILBOX_IMAP_HOST"}

    mail_id = str(arguments.get("id", "")).strip()
    if not mail_id:
        return {"ok": False, "error": "id is required for action=read_email"}
    mailbox = arguments.get("mailbox", "INBOX")

    with imaplib.IMAP4_SSL(cfg["imap_host"], cfg["imap_port"]) as imap:
        imap.login(cfg["email"], cfg["password"])
        status, _ = imap.select(mailbox, readonly=True)
        if status != "OK":
            return {"ok": False, "error": f"Cannot open mailbox: {mailbox}"}

        status, msg_data = imap.uid("fetch", mail_id.encode("utf-8"), "(RFC822)")
        if status != "OK" or not msg_data or not isinstance(msg_data[0], tuple):
            return {"ok": False, "error": f"Could not fetch email with id {mail_id}"}

        msg = email.message_from_bytes(msg_data[0][1])
        return {
            "ok": True,
            "protocol": "imap",
            "id": mail_id,
            "mailbox": mailbox,
            "from": decode_mime_header(msg.get("From", "")),
            "to": decode_mime_header(msg.get("To", "")),
            "subject": decode_mime_header(msg.get("Subject", "")),
            "date": msg.get("Date", ""),
            "body": extract_plain_text(msg),
        }


def read_email_pop3(arguments, cfg):
    if not cfg["pop3_host"]:
        return {"ok": False, "error": "Missing MAILBOX_POP3_HOST"}

    mail_id_raw = str(arguments.get("id", "")).strip()
    if not mail_id_raw:
        return {"ok": False, "error": "id is required for action=read_email"}

    try:
        mail_id = int(mail_id_raw)
    except ValueError:
        return {"ok": False, "error": "For POP3, id must be a numeric message index."}

    with poplib.POP3_SSL(cfg["pop3_host"], cfg["pop3_port"], timeout=30) as pop:
        pop.user(cfg["email"])
        pop.pass_(cfg["password"])

        _, lines, _ = pop.retr(mail_id)
        msg = email.message_from_bytes(b"\n".join(lines))

        return {
            "ok": True,
            "protocol": "pop3",
            "id": str(mail_id),
            "from": decode_mime_header(msg.get("From", "")),
            "to": decode_mime_header(msg.get("To", "")),
            "subject": decode_mime_header(msg.get("Subject", "")),
            "date": msg.get("Date", ""),
            "body": extract_plain_text(msg),
        }


def send_email(arguments, cfg):
    to_addr = str(arguments.get("to", "")).strip()
    subject = str(arguments.get("subject", "")).strip()
    body = str(arguments.get("body", "")).strip()

    if not to_addr:
        return {"ok": False, "error": "to is required for action=send_email"}
    if not subject:
        return {"ok": False, "error": "subject is required for action=send_email"}
    if not body:
        return {"ok": False, "error": "body is required for action=send_email"}

    msg = EmailMessage()
    msg["From"] = cfg["email"]
    msg["To"] = to_addr
    msg["Subject"] = subject
    msg.set_content(body)

    with smtplib.SMTP(cfg["smtp_host"], cfg["smtp_port"], timeout=30) as smtp:
        smtp.ehlo()
        smtp.starttls(context=ssl.create_default_context())
        smtp.ehlo()
        smtp.login(cfg["email"], cfg["password"])
        smtp.send_message(msg)

    return {
        "ok": True,
        "sent": True,
        "to": to_addr,
        "subject": subject,
        "sent_at_utc": datetime.now(timezone.utc).isoformat(),
    }


def main():
    try:
        arguments = json.loads(sys.argv[1] if len(sys.argv) > 1 else "{}")
        action = arguments.get("action")
        protocol = str(arguments.get("protocol", "imap")).lower().strip() or "imap"
        cfg = env_config()

        if action == "list_inbox":
            out = list_inbox_pop3(arguments, cfg) if protocol == "pop3" else list_inbox_imap(arguments, cfg)
        elif action == "read_email":
            out = read_email_pop3(arguments, cfg) if protocol == "pop3" else read_email_imap(arguments, cfg)
        elif action == "send_email":
            out = send_email(arguments, cfg)
        else:
            out = {"ok": False, "error": f"Unsupported action: {action}"}
    except Exception as exc:
        out = {"ok": False, "error": f"{type(exc).__name__}: {exc}"}

    print(json.dumps(out))


if __name__ == "__main__":
    main()
"#;

#[async_trait]
impl Skill for EmailMailboxSkill {
    fn definition(&self) -> LlmToolDefinition {
        LlmToolDefinition {
            name: "email_mailbox".to_string(),
            description: Some(
                "Read and send emails via generic IMAP/POP3/SMTP servers from the Docker worker. \
                 Use action=list_inbox/read_email/send_email; set protocol=imap or pop3 for inbox reading."
                    .to_string(),
            ),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list_inbox", "read_email", "send_email"],
                        "description": "Action to run."
                    },
                    "protocol": {
                        "type": "string",
                        "enum": ["imap", "pop3"],
                        "description": "For list_inbox/read_email: mailbox protocol. Default: imap."
                    },
                    "mailbox": {
                        "type": "string",
                        "description": "For IMAP list/read: folder name, e.g. INBOX. Ignored for POP3."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "For list_inbox: number of newest emails to return (1-50)."
                    },
                    "id": {
                        "type": "string",
                        "description": "For read_email: message id from list_inbox output (IMAP UID or POP3 index)."
                    },
                    "to": {
                        "type": "string",
                        "description": "For send_email: recipient address."
                    },
                    "subject": {
                        "type": "string",
                        "description": "For send_email: subject line."
                    },
                    "body": {
                        "type": "string",
                        "description": "For send_email: plain-text body."
                    }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: &Value) -> String {
        let args_json = arguments.to_string();

        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(45),
            Command::new("python3")
                .arg("-c")
                .arg(MAILBOX_BRIDGE)
                .arg(&args_json)
                .output(),
        )
        .await
        {
            Ok(Ok(out)) => out,
            Ok(Err(err)) => {
                return json!({
                    "ok": false,
                    "error": format!("Failed to start python bridge: {}", err),
                })
                .to_string();
            }
            Err(_) => {
                return json!({
                    "ok": false,
                    "error": "Mailbox command timed out after 45 seconds.",
                })
                .to_string();
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        if output.status.success() {
            if serde_json::from_str::<Value>(&stdout).is_ok() {
                stdout
            } else {
                json!({
                    "ok": false,
                    "error": "Mailbox bridge returned non-JSON output.",
                    "stdout": stdout,
                    "stderr": stderr,
                })
                .to_string()
            }
        } else {
            json!({
                "ok": false,
                "error": format!("Mailbox bridge exited with code {:?}", output.status.code()),
                "stdout": stdout,
                "stderr": stderr,
            })
            .to_string()
        }
    }
}
