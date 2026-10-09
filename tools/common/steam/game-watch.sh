# `cargo xtask test-steam-game` (docs/STEAM.md): started by hyprix's
# exec-once beside desktop.sh and store-watch.sh, as root. Once the client
# has signed in, it hands the running client steam://install/<app>, says
# how the app's manifest changes until Steam has installed it, then hands
# it steam://rungameid/<app> and says when the game's process runs and
# ends. The gate does the rest on the screen: the dialogs Steam opens, and
# the game's window.
export PATH=/bin:/data/usr/bin
app=380840
program=teeworlds
S=/data/steam
log=$S/logs/connection_log.txt
manifest=$S/steamapps/appmanifest_$app.acf
# What a second `steam` started by the client's user does with an address:
# hand it to the running client and exit (client.sh with an argument).
hand() {
    echo "steam-game: handing the client $1"
    ( su -p -s /bin/sh ferrix -c "/bin/busybox sh /steam/client.sh $1" 2>&1 \
        | sed 's/^/steam-game: hand: /'
      echo "steam-game: handed $1" ) &
}
# A field of the manifest, a line `"<name>" "<value>"`; nothing before
# Steam has written it.
field() {
    [ -f $manifest ] && sed -n "s/^[[:space:]]*\"$1\"[[:space:]]*\"\([^\"]*\)\".*/\1/p" $manifest | head -n 1
}
# The library on tmpfs rather than the btrfs volume, set up before the
# client starts (desktop.sh waits for the X server first): on 2026-10-01
# Steam staged its download on /data and then stood still for half an hour,
# and a find over steamapps/downloading on the volume never returned. On
# tmpfs that tells the volume's write path from the rest; the find hung
# on tmpfs as well, so that walk is no evidence about the volume.
mkdir -p /tmp/steamapps
if [ -d $S/steamapps ] && [ ! -L $S/steamapps ]; then
    cp -a $S/steamapps/. /tmp/steamapps/ && rm -rf $S/steamapps
fi
ln -sfn /tmp/steamapps $S/steamapps
chown -R 1000:1000 /tmp/steamapps
chown -h 1000:1000 $S/steamapps
echo "steam-game: the library is on tmpfs: $(ls -ld $S/steamapps)"
until [ -f $log ] && grep -q "RecvMsgClientLogOnResponse() : \[[^]]*\] 'OK'" $log; do sleep 5; done
echo "steam-game: the client has signed in"
# The gate judges the store before the game; an install dialog over the
# store would stand in its way.
sleep 120
# What the client says of the install and the start: its console log and
# its content log, from here on.
for name in console_log content_log compat_log gameprocess_log; do
    ( until [ -f $S/logs/$name.txt ]; do sleep 2; done
      tail -n 0 -F $S/logs/$name.txt 2>/dev/null \
          | awk -v said="steam-game: $name: " '{ print said $0; fflush() }' ) &
done
# The friends list Steam opens beside its main window floats over its left
# half, where the Install dialog's button is, and Steam opens it again
# after it was closed once (2026-10-01): closed every few seconds until the
# game is installed.
/bin/hyprctl dispatch closewindow "title:^Friends List$" 2>&1 | sed 's/^/steam-game: closing the friends list: /'
( while [ ! -f /tmp/steam-game-installed ]; do
      sleep 5
      /bin/hyprctl clients 2>/dev/null | grep -q "title: Friends List$" \
          && /bin/hyprctl dispatch closewindow "title:^Friends List$" > /dev/null 2>&1 \
          && echo "steam-game: closed the friends list again"
  done ) &
# The account must have the game in its library already (docs/STEAM.md
# §1): for a game it does not have, steam://install does nothing at all.
hand steam://install/$app
asked=$(date +%s)
told=
said=
while :; do
    if [ ! -f $manifest ] && [ -z "$told" ] && [ $(( $(date +%s) - asked )) -ge 180 ]; then
        echo "steam-game: no manifest 180s after the install was asked for"
        told=1
    fi
    state=$(field StateFlags)
    now="state ${state:-none}, downloaded $(field BytesDownloaded) of $(field BytesToDownload), staged $(field BytesStaged) of $(field BytesToStage)"
    if [ "$now" != "$said" ]; then
        echo "steam-game: manifest: $now"
        said=$now
    fi
    # 4 is fully installed, with nothing more to do.
    [ "$state" = 4 ] && break
    # What the processors are doing while Steam downloads, every 30s: the
    # busiest processes.
    if [ $(( ($(date +%s) - asked) % 30 )) -lt 5 ]; then
        top -b -n 1 | sed -n '2p;5,9p' | sed 's/^/steam-game: top: /'
    fi
    sleep 5
done
touch /tmp/steam-game-installed
dir=$S/steamapps/common/$(field installdir)
# The line the gate waits for at once; the sizes after it, in the background:
# a walk over a directory Steam writes into hung on 2026-10-01.
echo "steam-game: installed in $dir"
( echo "steam-game: installed: $(du -sm "$dir" | cut -f1) MiB, $(find "$dir" -type f | wc -l) files" ) &
hand steam://rungameid/$app
running=
while :; do
    pid=$(pidof $program)
    if [ -n "$pid" ] && [ -z "$running" ]; then
        echo "steam-game: $program runs, pid $pid"
        for p in $pid; do
            echo "steam-game: pid $p exe $(readlink /proc/$p/exe), cmdline $(tr '\0' ' ' < /proc/$p/cmdline)"
        done
    elif [ -z "$pid" ] && [ -n "$running" ]; then
        echo "steam-game: $program has ended"
    fi
    running=$pid
    sleep 2
done
