#!/bin/bash
# MACHINE-SPECIFIC: written for the N150 bring-up from the customer's Windows
# PC (2026-10-15, docs/N150.md §13.1). It hardcodes the box's keyboard's USB id
# (1a2c:2124); run on Linux on the box. Adjust those before running it
# anywhere else.
#
cd /sys/bus/hid/devices || exit 1
for d in 0003:1A2C:2124.*; do
    echo "== $d"
    od -An -tx1 -v "$d/report_descriptor"
done
echo "== hid drivers"
for d in 0003:1A2C:2124.*; do echo "$d -> $(basename "$(readlink -f "$d/driver")")"; done
