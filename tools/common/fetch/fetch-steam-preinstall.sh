#!/usr/bin/env bash
# Install Steam apps on the host with steamcmd, into a tree that
# `cargo xtask run-compositor --everything --steam-preinstall` (and
# `test-steam-game --steam-preinstall`) merges into the everything volume
# at `/data/steam/steamapps`: Steam in the guest finds each app's manifest
# at StateFlags 4, fully installed, and neither downloads nor stages it
# (docs/STEAM.md §7, item 7: the guest's staging stood still).
#
# By default Teeworlds (380840), the Steam Linux Runtime 1.0 (scout,
# 1070560), which Steam adds to the install of a `native` game, and the
# Steam Linux Runtime 2.0 (soldier, 1391110), which Steam downloads before
# it launches the game (scout runs on soldier). The apps
# must be owned by the account: Teeworlds is free, and
# `steamcmd +login <account> <password> +app_license_request 380840 +quit`
# claims it.
#
# The account is the gate's, from ~/.config/ferrix/steam-test-account
# (name and password, one a line; or FERRIX_STEAM_ACCOUNT_FILE), with
# Steam Guard off. Both go to steamcmd in a runscript readable by its owner
# alone, removed on exit, and every log line is redacted of the name.
#
# steamcmd runs on the host, natively: it needs the host's i386 loader at
# /lib/ld-linux.so.2. It is FERRIX_STEAMCMD_HOST (default
# ~/.local/share/ferrix/steamcmd-host), copied from the steamcmd volume's
# tree (fetch-steamcmd.sh) when missing; it updates itself there.
#
# Writes $FERRIX_STEAM_PREINSTALL (default
# ~/.local/share/ferrix/steam-preinstall): `tree/steam/steamapps` and the
# stamp `steam-preinstall.stamp` xtask compares with the volume's age.
#
# Usage: tools/common/fetch/fetch-steam-preinstall.sh [appid...]

set -euo pipefail

out=${FERRIX_STEAM_PREINSTALL:-$HOME/.local/share/ferrix/steam-preinstall}
host=${FERRIX_STEAMCMD_HOST:-$HOME/.local/share/ferrix/steamcmd-host}
account=${FERRIX_STEAM_ACCOUNT_FILE:-$HOME/.config/ferrix/steam-test-account}
apps=("$@")
[ ${#apps[@]} -gt 0 ] || apps=(380840 1070560 1391110)

[ -e /lib/ld-linux.so.2 ] \
    || { echo "fetch-steam-preinstall: no i386 loader at /lib/ld-linux.so.2: steamcmd cannot run on this host" >&2; exit 1; }
[ -r "$account" ] \
    || { echo "fetch-steam-preinstall: no account file at $account (docs/STEAM.md §1)" >&2; exit 1; }
if [ ! -x "$host/steamcmd.sh" ]; then
    from=${FERRIX_STEAMCMD_VOLUME:-$HOME/.local/share/ferrix/steamcmd}/tree/steamcmd
    [ -x "$from/steamcmd.sh" ] \
        || { echo "fetch-steam-preinstall: no steamcmd at $host or $from: run fetch-steamcmd.sh" >&2; exit 1; }
    mkdir -p "$host"
    cp -a "$from/." "$host/"
fi

name=$(sed -n 1p "$account")
redact() { sed -u "s/$(printf '%s' "$name" | sed 's/[]\/$*.^[]/\\&/g')/<account>/g"; }

library=$out/tree/steam
mkdir -p "$out/apps" "$library/steamapps/common"
script=$(mktemp "$out/runscript.XXXXXX")
trap 'rm -f "$script"' EXIT
chmod 600 "$script"

for app in "${apps[@]}"; do
    # One install directory per app: force_install_dir puts the app's files
    # at its top and the manifest under steamapps/.
    dir=$out/apps/$app
    mkdir -p "$dir"
    {
        echo "@sSteamCmdForcePlatformType linux"
        echo "force_install_dir $dir"
        printf 'login %s %s\n' "$name" "$(sed -n 2p "$account")"
        echo "app_update $app validate"
        echo quit
    } > "$script"
    echo "fetch-steam-preinstall: installing app $app with steamcmd"
    status=0
    (cd "$host" && ./steamcmd.sh +runscript "$script") 2>&1 | redact \
        | grep -E "Success|ERROR|Error|FAILED|state \(0x" || true
    manifest=$dir/steamapps/appmanifest_$app.acf
    [ -f "$manifest" ] && grep -q '"StateFlags"[[:space:]]*"4"' "$manifest" \
        || { echo "fetch-steam-preinstall: app $app is not fully installed (no manifest at StateFlags 4)" >&2; exit 1; }
    installdir=$(sed -n 's/^[[:space:]]*"installdir"[[:space:]]*"\([^"]*\)".*/\1/p' "$manifest" | head -n 1)
    [ -n "$installdir" ] || { echo "fetch-steam-preinstall: $manifest names no installdir" >&2; exit 1; }
    # Steam's own library layout: the manifest in steamapps, the files in
    # steamapps/common/<installdir>. Hard links: one copy on the host.
    rm -rf "${library:?}/steamapps/common/$installdir"
    mkdir -p "$library/steamapps/common/$installdir"
    (cd "$dir" && find . -mindepth 1 -maxdepth 1 ! -name steamapps -exec cp -al {} "$library/steamapps/common/$installdir/" \;)
    cp "$manifest" "$library/steamapps/"
    echo "fetch-steam-preinstall: app $app in steamapps/common/$installdir ($(du -sm "$library/steamapps/common/$installdir" | cut -f1) MiB)"
done

touch "$out/steam-preinstall.stamp"
echo "fetch-steam-preinstall: $library/steamapps ready; --steam-preinstall merges it"
