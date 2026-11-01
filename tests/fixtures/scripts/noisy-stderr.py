#!/usr/bin/env python3
# Writes to stderr but exits 0: a successful run that happens to be
# chatty must not be turned into an error.

import sys

sys.stderr.write("a benign warning\n")
