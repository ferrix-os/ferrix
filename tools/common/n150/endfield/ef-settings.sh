#!/bin/sh
# ef-settings.sh: Arknights: Endfield's display settings in a Wine prefix,
# read or set while the game is NOT running (ef-perf-sw, 2026-10-19).
# For the N150 box; touches only the prefix's registry (the game's own
# PlayerPrefs key) and makes a backup first. Never the game's files.
#
#   sh ef-settings.sh dump              what the game stored, to stdout and $OUT
#   sh ef-settings.sh set W H           Unity's window size W x H, windowed,
#                                       Unity quality level 0; backup first
#   sh ef-settings.sh restore FILE      put a backup of user.reg back
#
# Env: WINEPREFIX (default: the first that exists of ferrix's Proton prefix
# /home/ferrix/cd-dwp/pfx and ~ferrix's and ~root's gryphlink prefix),
# PROTON (default /data/dwproton, else /data/proton),
# OUT (default /home/ferrix/ef-settings-<time>).
#
# Where the game keeps them. Endfield is Unity 2021.3 (IL2CPP); its
# Endfield_Data/app.info says company "Gryphline", product "Endfield", so
# Unity's PlayerPrefs are HKCU\Software\Gryphline\Endfield, each value
# named KEY_h<djb2-xor hash of KEY>. Unity reads these at start:
#   Screenmanager Resolution Width_h182942802     REG_DWORD
#   Screenmanager Resolution Height_h2627697771   REG_DWORD
#   Screenmanager Fullscreen mode_h3630240806     REG_DWORD 3 = windowed
#   Screenmanager Resolution Use Native_h1405027254 REG_DWORD 0
#   UnityGraphicsQuality_h1669003810              REG_DWORD (Unity's level)
# The game's own options (quality tiers per setting id, TAAU render scale,
# frame rate) are its own tables' (GetSavedQualitySubSettingTierBySettingId,
# SetFrameRate, "[GraphicsResolution] ... re-applying resolution" in its
# metadata) and their key names are not known until a run writes them:
# `dump` after one session in the game shows them, and the next version of
# this script sets them. The game also picks its lowest tier for a device
# its table does not list ("doesn't exist in our table, use lowest tier
# setting") and lowers quality by itself on a low average frame rate
# ("Quality level Reduce from {0} to {1} avgFps{2}"), so on llvmpipe the
# defaults are likely low already; the lever left is the resolution.
#
# Launch flags do the same for one start and change nothing stored; add to
# the Endfield.exe line (ef-game.sh): -screen-width W -screen-height H
# -screen-fullscreen 0
set -u
cmd=${1:-dump}
if [ -z "${WINEPREFIX:-}" ]; then
    for p in /home/ferrix/cd-dwp/pfx /home/ferrix/.local/share/gryphlink/prefix /root/.local/share/gryphlink/prefix; do
        [ -e "$p/user.reg" ] && { WINEPREFIX=$p; break; }
    done
fi
[ -n "${WINEPREFIX:-}" ] && [ -e "$WINEPREFIX/user.reg" ] || { echo "ef-settings: no prefix (set WINEPREFIX)"; exit 2; }
export WINEPREFIX
if [ -z "${PROTON:-}" ]; then
    if [ -x /data/dwproton/files/bin/wine ]; then PROTON=/data/dwproton; else PROTON=/data/proton; fi
fi
export WINESERVER=$PROTON/files/bin/wineserver
export LD_LIBRARY_PATH=$PROTON/files/lib/x86_64-linux-gnu:$PROTON/files/lib/i386-linux-gnu
export WINEDLLPATH=$PROTON/files/lib/vkd3d:$PROTON/files/lib/wine
export WINEDEBUG=-all
wine=$PROTON/files/bin/wine
key='HKCU\Software\Gryphline\Endfield'
stamp=$(date +%Y%m%d-%H%M%S)
OUT=${OUT:-/home/ferrix/ef-settings-$stamp}

# The game, its launcher and any Wine of this prefix must be stopped: a
# running wineserver keeps the registry in memory and writes it back.
# argv[0] only, so that no shell whose command line names them matches.
# EF_FORCE=1 skips the check (a wineserver of another prefix, say).
for f in /proc/[0-9]*/cmdline; do
    [ "${EF_FORCE:-0}" = 1 ] && break
    a0=$( { tr '\0' '\n' < "$f"; } 2>/dev/null | head -n 1) || continue
    case "$a0" in
        *Endfield.exe|*Launcher.exe|*Games.exe|*/wineserver|wineserver)
            echo "ef-settings: still running: ${f%/cmdline} $a0"; echo "ef-settings: close the game and the launcher first"; exit 3 ;;
    esac
done

case "$cmd" in
dump)
    mkdir -p "$OUT"
    echo "ef-settings: prefix $WINEPREFIX, Wine $PROTON, out $OUT"
    "$wine" reg export "$key" 'Z:\tmp\ef-settings.reg' /y > /dev/null 2>&1
    "$WINESERVER" -w
    if [ -s /tmp/ef-settings.reg ]; then
        mv /tmp/ef-settings.reg "$OUT/playerprefs-utf16.reg"
    fi
    # The same section straight from user.reg, in case reg export failed.
    awk '/^\[Software\\\\Gryphline\\\\Endfield\]/{f=1} /^\[/{if(!/Gryphline\\\\Endfield/)f=0} f' \
        "$WINEPREFIX/user.reg" > "$OUT/user-reg-section.txt"
    echo "ef-settings: $(grep -c '^"' "$OUT/user-reg-section.txt") values under $key"
    head -80 "$OUT/user-reg-section.txt"
    ll="$WINEPREFIX/drive_c/users/steamuser/AppData/LocalLow/Gryphline/Endfield"
    [ -d "$ll" ] || ll=$(ls -d "$WINEPREFIX"/drive_c/users/*/AppData/LocalLow/Gryphline/Endfield 2>/dev/null | head -1)
    if [ -n "$ll" ] && [ -d "$ll" ]; then
        echo "ef-settings: $ll"
        find "$ll" -maxdepth 3 -type f -size -2M -exec ls -l {} \; | tee "$OUT/locallow.txt" | head -60
        # Small text-like files that may hold settings, copied (not moved).
        find "$ll" -maxdepth 3 -type f -size -256k \( -iname '*set*' -o -iname '*config*' -o -iname '*.json' -o -iname '*.ini' -o -iname '*.txt' \) \
            -exec cp {} "$OUT/" \; 2>/dev/null
    else
        echo "ef-settings: no LocalLow/Gryphline/Endfield in the prefix"
    fi
    ;;
set)
    w=${2:?width}; h=${3:?height}
    cp "$WINEPREFIX/user.reg" "$WINEPREFIX/user.reg.ef-settings-$stamp"
    echo "ef-settings: backup $WINEPREFIX/user.reg.ef-settings-$stamp"
    add() { "$wine" reg add "$key" /v "$1" /t REG_DWORD /d "$2" /f > /dev/null 2>&1 || echo "ef-settings: reg add $1 failed"; }
    add 'Screenmanager Resolution Width_h182942802' "$w"
    add 'Screenmanager Resolution Height_h2627697771' "$h"
    add 'Screenmanager Resolution Window Width_h2524650974' "$w"
    add 'Screenmanager Resolution Window Height_h1684712807' "$h"
    add 'Screenmanager Fullscreen mode_h3630240806' 3
    add 'Screenmanager Resolution Use Native_h1405027254' 0
    add 'UnityGraphicsQuality_h1669003810' 0
    "$WINESERVER" -w
    echo "ef-settings: set ${w}x${h} windowed, Unity quality 0:"
    grep -E 'Screenmanager|UnityGraphicsQuality' "$WINEPREFIX/user.reg"
    ;;
restore)
    b=${2:?backup file}
    [ -s "$b" ] || { echo "ef-settings: $b is empty or missing"; exit 2; }
    cp "$WINEPREFIX/user.reg" "$WINEPREFIX/user.reg.before-restore-$stamp"
    cp "$b" "$WINEPREFIX/user.reg"
    echo "ef-settings: restored $b (previous at user.reg.before-restore-$stamp)"
    ;;
*) echo "usage: ef-settings.sh dump | set W H | restore FILE"; exit 2 ;;
esac
