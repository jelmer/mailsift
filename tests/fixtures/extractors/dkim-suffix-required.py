#!/usr/bin/env python3
# Test fixture: emits a .receipt.json with a fixed merchant and
# orderNumber so the positive DKIM-suffix-match test can assert a
# specific output path. The negative test relies on this script
# NOT being invoked.

from __future__ import annotations

import json
import sys
from pathlib import Path


def main() -> int:
    sys.stdin.buffer.read()
    body = {
        "@type": "Order",
        "merchant": "Fixture Shop",
        "orderNumber": "ORD-42",
        "orderDate": "2026-05-01",
    }
    Path("fixture-shop-ORD-42.receipt.json").write_text(json.dumps(body))
    return 0


if __name__ == "__main__":
    sys.exit(main())
