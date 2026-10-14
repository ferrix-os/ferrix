# Ferrix guest: "name pid utime stime" (clock ticks) for the processes of interest, the cpu line, the time.
snap() {
  for p in $(pgrep -f "$2"); do
    [ -r /proc/$p/stat ] && echo "$1 $p $(cut -d' ' -f14,15 /proc/$p/stat)"
  done
}
snap teeworlds '^./teeworlds cl_'
snap teeworlds_srv 'teeworlds_srv sv_map'
snap hyprix '^/bin/hyprix'
snap virgl_test_server '^virgl_test_server'
snap yserver '[/]data/yserver/yserver'
snap chrome 'chrome-window/chrome|^/proc/self/exe --type'
snap pattern '^/bin/pattern'
snap waybar '^waybar'
echo "nvrm 9 $(cut -d' ' -f14,15 /proc/9/stat)"
head -n 1 /proc/stat
date +%s
