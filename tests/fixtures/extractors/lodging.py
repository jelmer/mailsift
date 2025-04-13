#!/usr/bin/env python3
# Test fixture: emit a LodgingReservation .reservation.json with a
# fixed reservationNumber + check-in/out so the Rust reservation
# converter has a deterministic input to render into an ICS VEVENT.

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> int:
    sys.stdin.buffer.read()

    body = {
        "@type": "LodgingReservation",
        "reservationNumber": "LDG-7777",
        "checkinTime": "2026-04-10T15:00:00",
        "checkoutTime": "2026-04-12T11:00:00",
        "reservationFor": {
            "@type": "LodgingBusiness",
            "name": "Fixture Inn",
            "address": "1 Example Street, Amsterdam, NL",
        },
    }
    Path("LDG-7777.reservation.json").write_text(json.dumps(body))
    return 0


if __name__ == "__main__":
    sys.exit(main())
