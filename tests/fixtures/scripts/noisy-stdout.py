#!/usr/bin/env python3
# Writes far more to stdout than a pipe buffer holds (64KiB on Linux),
# so the parent must drain the pipe concurrently or the child blocks
# in write() forever.

import sys

sys.stdout.write("x" * (2 * 1024 * 1024))
