#!/usr/bin/env python3
# Test fixture: copy every text/calendar attachment out as
# <message-id-or-counter>.event.ics. Kept stdlib-only so the fixture
# survives extractors/ moving to its own repo.

from __future__ import annotations

import email
import email.policy
import re
import sys
from pathlib import Path

SAFE = re.compile(r"[^A-Za-z0-9_.+-]+")


def main() -> int:
    msg = email.message_from_bytes(sys.stdin.buffer.read(), policy=email.policy.default)
    index = 0
    for part in msg.walk():
        if part.get_content_type() != "text/calendar":
            continue
        body = part.get_payload(decode=True)
        if body is None:
            continue
        raw_name = part.get_filename() or msg.get("Message-ID") or f"event-{index}"
        stem = raw_name.rsplit(".", 1)[0] if "." in raw_name else raw_name
        stem = stem.strip("<>")
        slug = SAFE.sub("-", stem).strip("-") or f"event-{index}"
        Path(f"{slug}.event.ics").write_bytes(body)
        index += 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
