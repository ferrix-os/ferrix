# The Steam client's half of `cargo xtask run-steam` (docs/STEAM.md), run by
# run.sh as uid 1000: as root the client moves its effective uid to the
# home's owner partway through, and GTK2's setuid check then exits.
#
# It starts Valve's 32-bit ubuntu12_32/steam directly, with the environment
# steam.sh would give it, rather than through steam.sh: steam.sh stops at
# scout's user-namespace check (docs/I386.md, I5b). From the bootstrap the
# first start downloads and installs the client and exits 42 to be started
# again, as steam.sh would.
export PATH=/steam/bin:/bin:/data/usr/bin HOME=/data/home USER=ferrix LANG=C.UTF-8
export DISPLAY=:0
S=/data/steam
RT=$S/ubuntu12_32/steam-runtime
export STEAM_RUNTIME=$RT SDL_VIDEO_X11_DGAMOUSE=0 SRT_LOG_LEVEL_PREFIX=1 STEAM_RUNTIME_LOGGER=0
export SYSTEM_LD_LIBRARY_PATH=/usr/lib/i386-linux-gnu SYSTEM_PATH=$PATH
# The 32-bit client draws its own UI with GLX: Debian's i386 Mesa, llvmpipe.
export LIBGL_DRIVERS_PATH=/usr/lib/i386-linux-gnu/dri:/usr/lib/x86_64-linux-gnu/dri
export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe
export STEAM_RUNTIME_LIBRARY_PATH=$RT/pinned_libs_32:$RT/pinned_libs_64:/usr/lib/i386-linux-gnu:/usr/lib/x86_64-linux-gnu:$RT/lib/i386-linux-gnu:$RT/usr/lib/i386-linux-gnu:$RT/lib/x86_64-linux-gnu:$RT/usr/lib/x86_64-linux-gnu:$RT/lib:$RT/usr/lib
export LD_LIBRARY_PATH=$S/ubuntu12_32:$S/ubuntu12_32/panorama:$STEAM_RUNTIME_LIBRARY_PATH
# The stand-ins run-steam carries: the steamrt64 entry point that runs
# steamwebhelper without pressure-vessel, and a logger that logs nothing.
export STEAM_RUNTIME_STEAMRT=/steam/steamrt STEAM_RUNTIME_SCOUT=/steam/scout
export STEAMSCRIPT=$S/steam.sh
export PATH=$RT/amd64/bin:$RT/amd64/usr/bin:$RT/usr/bin:$PATH
# With an argument, a steam:// address (`test-steam-game`'s game-watch.sh):
# hand it to the running client, as a second `steam` does, before anything
# below takes the running client's place (its pid file).
if [ $# -gt 0 ]; then
    exec $S/ubuntu12_32/steam "$@"
fi
mkdir -p $HOME/.steam/sdk32 $HOME/.steam/sdk64
ln -sfn $S $HOME/.steam/steam
ln -sfn $S $HOME/.steam/root
ln -sfn $S/ubuntu12_32 $HOME/.steam/bin32
ln -sfn $S/ubuntu12_64 $HOME/.steam/bin64
ln -sfn $S/ubuntu12_32 $HOME/.steam/bin
ln -sf $S/linux32/steamclient.so $HOME/.steam/sdk32/steamclient.so
ln -sf $S/linux64/steamclient.so $HOME/.steam/sdk64/steamclient.so
echo $$ > $HOME/.steam/steam.pid
cd $S
# steam.sh's unpack_runtime: the first start downloads scout's runtime as
# steam-runtime.tar.xz beside the bootstrap's stub of it, and steam.sh
# unpacks it before the restart. Without it the 32-bit steamui.so finds no
# i386 libXtst or libXrandr and the client ends "Failed to load steamui.so".
# The volume's xz by name: busybox's, first in PATH, has no xz applet.
unpack_runtime() {
    archive=$S/ubuntu12_32/steam-runtime.tar.xz
    [ -f $archive ] && [ -f $archive.checksum ] || return 0
    [ -f $RT/checksum ] && [ "$(cat $archive.checksum)" = "$(cat $RT/checksum)" ] && return 0
    if [ "$(cd $S/ubuntu12_32 && md5sum steam-runtime.tar.xz)" != "$(cat $archive.checksum)" ]; then
        echo "steam-window: scout's runtime does not match its checksum"
        return 1
    fi
    echo "steam-window: unpacking scout's runtime"
    rm -rf $RT.tmp && mkdir $RT.tmp \
        && /data/usr/bin/tar -I /data/usr/bin/xz -xf $archive -C $RT.tmp \
        && rm -rf $RT.old && { [ ! -d $RT ] || mv $RT $RT.old; } \
        && mv $RT.tmp/* $S/ubuntu12_32/ && rm -rf $RT.tmp \
        && cp $archive.checksum $RT/checksum \
        || { echo "steam-window: unpacking scout's runtime failed"; return 1; }
}
# The Steam Linux Runtime steamwebhelper's libraries come from arrives
# unpacked, in steamrt64/pv-runtime/steam-runtime-steamrt. Its platform's
# files/ holds each library by its full name only: pressure-vessel's
# deployment adds the soname links, which steamwebhelper's loader looks for
# (libXdamage.so.1 and eleven more). Without pressure-vessel the volume's
# ldconfig makes them, without a cache; again each start, as an update may
# bring a new platform.
link_steamrt() {
    for files in $S/steamrt64/pv-runtime/steam-runtime-steamrt/steamrt*_platform_*/files; do
        [ -d $files ] || continue
        /data/sbin/ldconfig -n $files/lib/x86_64-linux-gnu $files/lib \
            || echo "steam-window: ldconfig -n in $files failed"
    done
}
# Native Linux games run directly (docs/STEAM.md §7): Steam starts a native
# game no tool is mapped to in the Steam Linux Runtime 1.0 (scout)
# container, app 1070560, whose pressure-vessel needs bubblewrap in a user
# namespace as this user (docs/NAMESPACES.md N5 to N7). The compatibility
# tool ferrix_direct, carried at /steam/compat, runs the game itself
# instead; each app in DIRECT_APPS gets a user mapping to it in config.vdf,
# which outranks Valve's (priority 250, as the client's own Properties page
# writes). Before each start: the client writes config.vdf back as it holds
# it, and it holds the mapping from the start on.
DIRECT_APPS=${FERRIX_DIRECT_APPS-380840}
install_direct() {
    mkdir -p $S/compatibilitytools.d/ferrix_direct
    cp /steam/compat/ferrix_direct/* $S/compatibilitytools.d/ferrix_direct/
    chmod 755 $S/compatibilitytools.d/ferrix_direct/run
    cfg=$S/config/config.vdf
    mkdir -p $S/config
    [ -f $cfg ] || printf '"InstallConfigStore"\n{\n\t"Software"\n\t{\n\t\t"Valve"\n\t\t{\n\t\t\t"Steam"\n\t\t\t{\n\t\t\t}\n\t\t}\n\t}\n}\n' > $cfg
    for app in $DIRECT_APPS; do
        # Already mapped to ferrix_direct: the app's block names it.
        awk -v app="\"$app\"" '$1 == app { getline; getline; if ($2 == "\"ferrix_direct\"") found = 1 } END { exit !found }' $cfg && continue
        awk -v app="$app" '
            function entry(t) {
                print t "\t\"" app "\""; print t "\t{"
                print t "\t\t\"name\"\t\t\"ferrix_direct\""; print t "\t\t\"config\"\t\t\"\""
                print t "\t\t\"priority\"\t\t\"250\""; print t "\t}"
            }
            { line = $0; key = $0; gsub(/^[ \t]+|[ \t]+$/, "", key) }
            key == "{" { name[++depth] = tolower(pending); path = path "/" name[depth]
                print line
                if (path == "/installconfigstore/software/valve/steam/compattoolmapping" && !done) {
                    t = line; sub(/\{.*/, "", t); entry(t); done = 1
                }
                next }
            key == "}" {
                if (path == "/installconfigstore/software/valve/steam" && !done) {
                    t = line; sub(/\}.*/, "", t)
                    print t "\t\"CompatToolMapping\""; print t "\t{"; entry(t "\t"); print t "\t}"
                    done = 1
                }
                path = substr(path, 1, length(path) - length(name[depth]) - 1); depth--; print line; next }
            { if (key ~ /^"[^"]*"$/) { pending = key; gsub(/"/, "", pending) } print line }
            END { if (!done) print "steam-window: no Steam block in config.vdf for " app > "/dev/stderr" }
        ' $cfg > $cfg.new && mv $cfg.new $cfg
        echo "steam-window: app $app mapped to ferrix_direct"
    done
}
n=0
while [ $n -lt 4 ]; do
    n=$((n + 1))
    unpack_runtime
    link_steamrt
    install_direct
    # Scout's pinned libraries, as steam.sh has its setup.sh make them, and
    # with the PATH steam.sh has then: without the runtime's directories.
    # With them, setup.sh finds the runtime's own zenity and pipes its
    # progress into it; that zenity does not load here (scout's gdk-pixbuf
    # imports _IO_getc, which ferrousli lacks), and pipefail turns the
    # loader's 127 into setup.sh's. setup.sh expects no runtime zenity at
    # this point, and without one reports its progress on stderr.
    PATH=$SYSTEM_PATH bash $RT/setup.sh > /tmp/setup.log 2>&1 || {
        echo "steam-window: scout's setup.sh exited $?; what it said:"
        # Less its progress, percentages between carriage returns, and the
        # line each pinned library gets, neither of which says why it
        # failed; each other line once.
        tr '\r' '\n' < /tmp/setup.log \
            | grep -v -E -e '^ *[0-9]+% *$' -e '^ *$' \
                -e '^setup\.sh\[[0-9]+\]: (Found newer|Forced use|Updating Steam)' \
            | awk '!seen[$0]++' | tail -n 20 | sed 's/^/steam-window: setup: /'
    }
    echo "steam-window: starting the client, try $n"
    # The updater's progress lines, several a second while it downloads,
    # are left out of the transcript (GNU grep, which can flush each line).
    ( $S/ubuntu12_32/steam -srt-logger-opened -no-cef-sandbox -cef-disable-gpu -cef-disable-gpu-compositing $STEAM_WINDOW_ARGS; echo $? > /tmp/steam-status ) 2>&1 \
        | env -u LD_LIBRARY_PATH -u LD_PRELOAD /data/usr/bin/grep --line-buffered -v -e 'Downloading update (' -e 'Set percent complete' -e '%] ' \
        | sed 's/^/steam-window: out: /'
    status=$(cat /tmp/steam-status)
    echo "steam-window: the client exited $status"
    [ "$status" = 42 ] || break
done
