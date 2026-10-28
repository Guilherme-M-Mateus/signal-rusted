#!/usr/bin/env python3
"""signal-rusted plugin: replies "pong" to "!ping" (see docs/plugins.md)."""
import json
import sys

for line in sys.stdin:
    event = json.loads(line)
    if event.get("type") == "message" and not event.get("outgoing") and event.get("body", "").strip() == "!ping":
        print(json.dumps({"type": "send", "chat": event["chat"], "body": "pong"}), flush=True)
