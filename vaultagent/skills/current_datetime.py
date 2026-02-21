#!/usr/bin/env python3
"""
Beispiel Python-Skill für VaultAgent.

Convention:
  python3 script.py --describe     → JSON Tool-Definition auf stdout
  python3 script.py --execute '{...}'  → JSON Ergebnis auf stdout
"""

import json
import sys
import datetime


DESCRIPTION = {
    "name": "current_datetime",
    "description": "Returns the current date and time.",
    "parameters": {
        "type": "object",
        "properties": {
            "format": {
                "type": "string",
                "description": "Optional: strftime format, e.g. '%Y-%m-%d %H:%M:%S'. Default: ISO 8601."
            }
        },
        "additionalProperties": False
    }
}


def execute(arguments: dict) -> dict:
    fmt = arguments.get("format")
    now = datetime.datetime.now()

    if fmt:
        try:
            formatted = now.strftime(fmt)
        except Exception as e:
            return {"ok": False, "error": f"Invalid format: {e}"}
    else:
        formatted = now.isoformat()

    return {"ok": True, "datetime": formatted}


def main():
    if len(sys.argv) < 2:
        print(json.dumps({"error": "Usage: script.py --describe | --execute '{...}'"}))
        sys.exit(1)

    command = sys.argv[1]

    if command == "--describe":
        print(json.dumps(DESCRIPTION))
    elif command == "--execute":
        args_json = sys.argv[2] if len(sys.argv) > 2 else "{}"
        try:
            arguments = json.loads(args_json)
        except json.JSONDecodeError as e:
            print(json.dumps({"ok": False, "error": f"Invalid JSON: {e}"}))
            sys.exit(1)
        result = execute(arguments)
        print(json.dumps(result))
    else:
        print(json.dumps({"error": f"Unknown command: {command}"}))
        sys.exit(1)


if __name__ == "__main__":
    main()
