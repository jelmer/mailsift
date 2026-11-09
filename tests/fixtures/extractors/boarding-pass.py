#!/usr/bin/env python3
# Test fixture: a boarding pass mailed after check-in, with no
# reservation beside it. Each pass names its booking in a same-slug
# `.ticket.json` instead; the second's record is malformed, and the
# third record has no pass.

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> int:
    sys.stdin.buffer.read()
    Path("fixture-air-fx456-2027-01-05.ticket.pdf").write_bytes(b"%PDF-1.4 pass one")
    Path("fixture-air-fx456-2027-01-05.ticket.json").write_text(
        json.dumps(
            {
                "reservationNumber": "FX9ZZ1",
                "underName": "Test User",
                "provider": "Fixture Air",
                "date": "2027-01-05",
            }
        )
    )
    Path("fixture-air-fx789-2027-01-05.ticket.pdf").write_bytes(b"%PDF-1.4 pass two")
    Path("fixture-air-fx789-2027-01-05.ticket.json").write_text('{"reservationNumber": ')
    Path("fixture-air-fx000-2027-01-05.ticket.json").write_text(
        json.dumps({"reservationNumber": "FX9ZZ1"})
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
