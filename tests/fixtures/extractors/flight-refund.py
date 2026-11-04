#!/usr/bin/env python3
# Test fixture: a refund confirmation that only names the booking, so
# it cancels the booking by reference rather than leg by leg.

import json
import sys
from pathlib import Path


def main() -> int:
    sys.stdin.buffer.read()
    body = {
        "@type": "FlightReservation",
        "reservationNumber": "FX7QT2",
        "reservationStatus": "ReservationCancelled",
    }
    Path("refund.reservation.json").write_text(json.dumps(body))
    return 0


if __name__ == "__main__":
    sys.exit(main())
