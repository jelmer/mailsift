#!/bin/sh
# Never finishes on its own: starts a background child (pid recorded in
# the file named on the first line of stdin), reports progress on
# stderr and then sleeps in the foreground.

read -r pidfile
sleep 60 &
echo $! > "$pidfile"
echo stuck on step 3 >&2
sleep 60
