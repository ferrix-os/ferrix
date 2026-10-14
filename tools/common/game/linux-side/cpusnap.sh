snap() { for p in $(pgrep -f "$2"); do [ -r /proc/$p/stat ] && echo "$1 $p $(cut -d' ' -f14,15 /proc/$p/stat)"; done; }
snap teeworlds '^./teeworlds cl_'
snap teeworlds_srv 'teeworlds_srv sv_map'
snap sway '^sway'
snap Xwayland 'Xwayland'
snap chrome 'chrome-window/chrome|^/proc/self/exe --type'
head -n 1 /proc/stat
date +%s
