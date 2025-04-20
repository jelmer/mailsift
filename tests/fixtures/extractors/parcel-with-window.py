#!/usr/bin/env python3
# Test fixture: emit one .parcel.json plus a sibling .reservation.json
# (EventReservation) so the test can pin both that the reservation
# converter renders an EventReservation into a VEVENT and that the
# sibling-year derivation picks up the reservation's date.

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> int:
    sys.stdin.buffer.read()

    parcel = {
        "@type": "ParcelDelivery",
        "trackingNumber": "WIN-9876",
        "deliveryStatus": "OutForDelivery",
        "provider": {"@id": "fixture-carrier", "name": "Fixture Carrier"},
    }
    Path("WIN-9876.parcel.json").write_text(json.dumps(parcel))

    reservation = {
        "@type": "EventReservation",
        "reservationNumber": "WIN-9876",
        "reservationFor": {
            "@type": "Event",
            "name": "Fixture delivery",
            "startDate": "2026-02-09T13:40:00",
            "endDate": "2026-02-09T14:40:00",
        },
    }
    Path("WIN-9876.reservation.json").write_text(json.dumps(reservation))
    return 0


if __name__ == "__main__":
    sys.exit(main())
