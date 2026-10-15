#!/bin/bash
# Stop polling the 1a2c:2124 keyboard: unbind usbhid from its interfaces,
# then watch the kernel log for a disconnect while keys are pressed.
for i in /sys/bus/usb/drivers/usbhid/*:1.*; do
    dev=$(readlink -f "$i/..")
    if [ "$(cat "$dev/idVendor" 2>/dev/null)" = 1a2c ]; then
        name=$(basename "$i")
        echo "$name" > /sys/bus/usb/drivers/usbhid/unbind && echo "unbound $name"
    fi
done
dmesg -C
echo "watching for 60 s"
timeout 60 dmesg -w | grep --line-buffered -iE 'usb|disconnect|1a2c|reset' &
sleep 61
echo "done; device still present:"
grep -l 1a2c /sys/bus/usb/devices/*/idVendor 2>/dev/null
