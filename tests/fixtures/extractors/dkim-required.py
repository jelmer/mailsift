#!/usr/bin/env python3
# Test fixture: emits a marker event if it runs at all. The test
# pins that DKIM gating skips this extractor on a spoofed message,
# so the *absence* of the marker file is the assertion.

from __future__ import annotations

import sys
from pathlib import Path


def main() -> int:
    sys.stdin.buffer.read()
    body = (
        "BEGIN:VCALENDAR\r\n"
        "VERSION:2.0\r\n"
        "BEGIN:VEVENT\r\n"
        "UID:dkim-marker@example.com\r\n"
        "SUMMARY:dkim-marker\r\n"
        "DTSTART:20260101T000000Z\r\n"
        "DTEND:20260101T010000Z\r\n"
        "END:VEVENT\r\n"
        "END:VCALENDAR\r\n"
    )
    Path("dkim-marker.event.ics").write_text(body)
    return 0


if __name__ == "__main__":
    sys.exit(main())
