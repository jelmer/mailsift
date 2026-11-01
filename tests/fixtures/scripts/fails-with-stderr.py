#!/usr/bin/env python3
# Exits non-zero after writing a recognisable line to stderr, which the
# error surfaced to the caller is expected to include.

import sys

sys.stderr.write("kaboom detail line\n")
sys.exit(2)
