#!/usr/bin/env python3
# Test fixture: emits a FlightReservation alongside a boarding-pass
# ticket blob, so the pipeline test can assert both the archived
# reservation JSON and the ticket's sidecar (which draws its booking
# reference and passenger name from the reservation beside it).

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> int:
    sys.stdin.buffer.read()
    body = {
        "@type": "FlightReservation",
        "reservationNumber": "FX7QT2",
        "underName": {"name": "J Vernooij"},
        "reservationFor": {
            "flightNumber": "123",
            "airline": {"iataCode": "FX", "name": "Fixture Air"},
            "departureTime": "2026-04-10T08:00:00Z",
            "arrivalTime": "2026-04-10T10:00:00Z",
        },
    }
    Path("flight.reservation.json").write_text(json.dumps(body))
    # Slug follows the documented <what-it-is>-<date> convention;
    # the blob keeps this name when filed.
    Path("fixture-air-fx123-2026-04-10.ticket.pdf").write_bytes(
        b"%PDF-1.4 fixture boarding pass"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
