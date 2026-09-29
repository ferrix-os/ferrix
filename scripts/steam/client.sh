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
# Launch-side workarounds for kernel gaps (scripts/steam/workarounds/).
export LD_PRELOAD='/data/steam-workarounds/$LIB/pipe2-direct.so'
# The stand-ins run-steam carries: the steamrt64 entry point that runs
# steamwebhelper without pressure-vessel, and a logger that logs nothing.
export STEAM_RUNTIME_STEAMRT=/steam/steamrt STEAM_RUNTIME_SCOUT=/steam/scout
export STEAMSCRIPT=$S/steam.sh
export PATH=$RT/amd64/bin:$RT/amd64/usr/bin:$RT/usr/bin:$PATH
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
n=0
while [ $n -lt 4 ]; do
    n=$((n + 1))
    unpack_runtime
    link_steamrt
    # Scout's pinned libraries, as steam.sh has its setup.sh make them.
    bash $RT/setup.sh > /dev/null 2>&1 || echo "steam-window: scout's setup.sh exited $?"
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
