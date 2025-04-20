#!/usr/bin/env python3
# Test fixture: emit a .parcel.json whose tracking number is fixed and
# whose deliveryStatus is whichever value follows "fixture-parcel:" in
# the subject. Exercises the parcels-target merge-by-tracking-number
# behaviour without a real-vendor extractor.

from __future__ import annotations

import email
import email.policy
import json
import sys
from pathlib import Path


def main() -> int:
    msg = email.message_from_bytes(sys.stdin.buffer.read(), policy=email.policy.default)
    subject = msg.get("Subject", "")
    _, _, status = subject.partition(":")
    status = status.strip() or "Unknown"

    body = {
        "@type": "ParcelDelivery",
        "trackingNumber": "FIXT-12345",
        "deliveryStatus": status,
        "provider": {
            "@id": "fixture-carrier",
            "name": "Fixture Carrier",
        },
    }
    Path("FIXT-12345.parcel.json").write_text(json.dumps(body))
    return 0


if __name__ == "__main__":
    sys.exit(main())
