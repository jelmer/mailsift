#!/bin/sh
# Leaves a background child holding stdout/stderr open after exiting,
# recording its pid in the file named on the first line of stdin.

read -r pidfile
sleep 60 &
echo $! > "$pidfile"
exit 0
