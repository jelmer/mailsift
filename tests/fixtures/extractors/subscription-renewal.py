#!/usr/bin/env python3
# Test fixture: emit a .subscription.json for a fixed subscription whose
# price is whatever follows "fixture-renewal:" in the subject. Exercises
# the subscriptions target keeping the record from the newest message
# without a real-vendor extractor.

from __future__ import annotations

import email
import email.policy
import json
import sys
from pathlib import Path


def main() -> int:
    msg = email.message_from_bytes(sys.stdin.buffer.read(), policy=email.policy.default)
    _, _, price = msg.get("Subject", "").partition(":")

    body = {
        "@type": "Offer",
        "name": "Fixture Music",
        "price": float(price),
        "priceCurrency": "GBP",
        "subscriptionDuration": "P1M",
    }
    Path("fixture-music.subscription.json").write_text(json.dumps(body))
    return 0


if __name__ == "__main__":
    sys.exit(main())
