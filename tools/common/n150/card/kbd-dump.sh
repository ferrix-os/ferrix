#!/bin/bash
cd /sys/bus/hid/devices || exit 1
for d in 0003:1A2C:2124.*; do
    echo "== $d"
    od -An -tx1 -v "$d/report_descriptor"
done
echo "== hid drivers"
for d in 0003:1A2C:2124.*; do echo "$d -> $(basename "$(readlink -f "$d/driver")")"; done
