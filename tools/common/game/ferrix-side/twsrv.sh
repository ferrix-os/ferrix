# Ferrix guest: the Teeworlds server as /steam/nvidia-watch.sh starts it.
cd /tmp/tw/tw
LD_LIBRARY_PATH=/tmp/tw/lib:/usr/lib/x86_64-linux-gnu HOME=/home/ferrix exec ./teeworlds_srv "sv_map dm1" > /tmp/lb/srv.out 2>&1
