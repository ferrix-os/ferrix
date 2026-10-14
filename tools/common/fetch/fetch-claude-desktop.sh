#!/usr/bin/env bash
# Fetch Anthropic's Claude desktop app for Linux, and the Debian libraries it
# needs that the `--everything` volume does not already carry, into a tree
# `cargo xtask run-compositor --everything` merges into that volume
# (docs/CLAUDE-CODE.md §7).
#
# Nothing here is built, and nothing of the package runs. `claude-desktop` is
# Anthropic's own Linux build, a 229 MB glibc Electron program, from
# Anthropic's apt repository. Its package is proprietary: Ferrix carries none
# of it -- no image, no repository and no package Ferrix publishes holds it --
# and this script fetches it on the machine it runs on, as
# fetch-claude-code.sh fetches Claude Code. Only the package's files are
# unpacked (`dpkg-deb -x`): its maintainer scripts, which would register
# Anthropic's apt repository on the host, are never run.
#
# The download is checked as apt checks it, from a pinned key down:
#   1. the repository's key, whose fingerprint must be [KEY_FINGERPRINT],
#      the one Anthropic's page names;
#   2. `InRelease`, whose signature gpgv must find good and made by that key;
#   3. `main/binary-amd64/Packages`, whose SHA-256 and size must be the ones
#      the signed `InRelease` lists;
#   4. the `.deb`, whose SHA-256 must be the one that `Packages` lists for the
#      pinned version -- and the pinned SHA-256 below as well, so a release
#      Anthropic signs later changes nothing here until Ferrix pins it.
#
# Beside the app: GTK 3, which the program links, PipeWire's client library,
# which its native binding links, and what those two need that the volumes
# merged before this one lack, all Debian 13's and pinned by SHA-256 from
# trixie's Packages file on 2026-10-14. GTK 3's dconf backend, PipeWire's SPA
# plugins and the icon-cache tool are left out: GLib keeps settings in memory
# without dconf, there is no PipeWire server to talk to, and nothing runs
# the tool. GTK's GSettings schemas are compiled here, as the package's
# trigger would, since GTK's file chooser stops the program without them.
#
# After unpacking, every library each ELF file in the tree needs is looked
# for in it and in the trees of the volumes it is merged onto
# ($CLAUDE_DESKTOP_BASE_TREES, colon-separated; by default those volumes'
# trees at their default places), so a dependency this list lacks fails here
# rather than on Ferrix. The one exception is Cowork's `virtiofsd`, whose
# libcap-ng and libseccomp are not fetched: Cowork needs a hypervisor in
# Ferrix and is not planned.
#
# Writes $FERRIX_CLAUDE_DESKTOP_VOLUME/tree and, once the tree is whole,
# claude-desktop.stamp beside it (default ~/.local/share/ferrix/claude-desktop).
# No image: the tree is only ever merged into the --everything volume. Needs
# curl, sha256sum, gpg, gpgv, dpkg-deb, readelf and glib-compile-schemas;
# no root.
#
# Usage: tools/common/fetch/fetch-claude-desktop.sh

set -euo pipefail

debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
repository=${CLAUDE_DESKTOP_APT:-https://downloads.claude.ai/claude-desktop/apt/stable}
key_url=${CLAUDE_DESKTOP_KEY:-https://downloads.claude.ai/claude-desktop/key.asc}
data=${XDG_DATA_HOME:-$HOME/.local/share}/ferrix
out=${FERRIX_CLAUDE_DESKTOP_VOLUME:-$data/claude-desktop}
base=${CLAUDE_DESKTOP_BASE_TREES:-$data/rustc/tree:$data/chrome/tree:$data/steamcmd/tree:$data/claude-code/tree:$data/steam-window/tree:$data/endfield-launcher-proton/tree}

# "Anthropic Claude Code Release Signing", the key Anthropic's Linux page
# (code.claude.com/docs/en/desktop-linux) names for its repository.
KEY_FINGERPRINT=31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE

# The stable channel's release on 2026-10-14, and its package's SHA-256 as
# that day's Packages index lists it.
CLAUDE_DESKTOP_VERSION=2.37337.1
CLAUDE_DESKTOP_SHA256=41745b0914f0ff51631a1ff1bb403b836bf5c995845cca6742dd39bca3a2da14

# Pool path and SHA-256 of each Debian 13 package.
debs=(
    "main/g/gtk+3.0/libgtk-3-0t64_3.24.49-3_amd64.deb 4aa8828f01365f937f3bf69f812c8107ee4c8a2b208ffdd7656e8de5de81d98f"
    "main/g/gtk+3.0/libgtk-3-common_3.24.49-3_all.deb 9161bd6b9933384073ac5bacd6a3bb1e2998877a605411d73041fb0b7fadd605"
    "main/p/pipewire/libpipewire-0.3-0t64_1.4.2-1_amd64.deb 68522d7dff91b0f759fe58e685e19422bc2bb43dc8d6fd41f025954da09a3d0f"
    "main/a/adwaita-icon-theme/adwaita-icon-theme_48.1-1_all.deb f60b0577aee314d43d6908ca7f0b8f07dc48e2bb766e7e25035330817e0e827c"
    "main/h/hicolor-icon-theme/hicolor-icon-theme_0.18-2_all.deb d91fef83a186d3fc9fff293bfd7da4f684f736d15459d258a5a0d251168c1a35"
    "main/c/cairo/libcairo-gobject2_1.18.4-1+b1_amd64.deb 4fcd432ebe8f93599bd05426f304e2f70409aad271fc6b4b3a2c6243c99c377c"
    "main/libc/libcloudproviders/libcloudproviders0_0.3.6-2_amd64.deb a626bb23ed9ebc4bc3ce446b552048de63d76768bf0fb492754965c5ee28a7e4"
    "main/c/colord/libcolord2_1.4.7-3_amd64.deb f11fd3b492781122edf0790f1e1fe24c51a41c67d4bb76cff84a9426c5654fa7"
    "main/libe/libepoxy/libepoxy0_1.5.10-2_amd64.deb 4c4c8024f2175086de65bca9fdc3fbb967f2863ebf249d489c66d0c8103dd3a3"
    "main/g/gdk-pixbuf/libgdk-pixbuf-2.0-0_2.42.12+dfsg-4+deb13u1_amd64.deb 94150fb955b2d3a9a5e054e31c5a4b048bad4a479207e86cde46fbc975c838b4"
    "main/g/gdk-pixbuf/libgdk-pixbuf2.0-common_2.42.12+dfsg-4+deb13u1_all.deb 9f0c64e33cf5bb3226a5a6ddf630dd76530596e476e6b47a8b27da444ff43a03"
    "main/p/pango1.0/libpangocairo-1.0-0_1.56.3-1_amd64.deb 6994729c5b7a9649661ec078cf039991f22eff44600543aa917894f83c89cd05"
    "main/p/pango1.0/libpangoft2-1.0-0_1.56.3-1_amd64.deb b506cf66f78ddf542ddb4bb4a68b3d748d41caa6e190e4480430a6630c1e5105"
    "main/l/lcms2/liblcms2-2_2.16-2+deb13u2_amd64.deb 32e53c2002ccea8c60c02779bea0ba49ac132fef702988a30d64af79b6617806"
    "main/libj/libjpeg-turbo/libjpeg62-turbo_2.1.5-4_amd64.deb 5504299ad0e3c0a9ca9f962a68a650a040db52f9f416a109e74df48d55601d3f"
    "main/t/tiff/libtiff6_4.7.0-3+deb13u3_amd64.deb 6bb2038f36817ae1ee6d33df03917b21835dcc2e0f3e3e80a71390e83c171173"
    "main/libd/libdeflate/libdeflate0_1.23-2_amd64.deb 5fe37fb79d53f5c91da6ba3ac4c1a8453e1f2b1ff0e0ea45189c5338bbfdbf82"
    "main/j/jbigkit/libjbig0_2.1-6.1+b2_amd64.deb 95cbc44561651e1b3385454df08e8f2efe84498f6963a85259be2cc2fe798709"
    "main/l/lerc/liblerc4_4.0.0+ds-5_amd64.deb 2e0c13c0ddbde12968c2c713c2e0474a5b78e90d5216031256a725e8c76aa31c"
    "main/libw/libwebp/libwebp7_1.5.0-0.1_amd64.deb c749744b04abdd1426b0c7718e516362c03a557c5151662092d6d4f2cef6e7c1"
    "main/libw/libwebp/libsharpyuv0_1.5.0-0.1_amd64.deb 51934833d909588f7761fd84362114e90d8d2d38af57edf5df5c4a705e037547"
)

# Libraries no file in the tree may be missing but these, needed by
# Cowork's virtiofsd alone.
COWORK_ONLY="libcap-ng.so.0 libseccomp.so.2"

say() { echo "fetch-claude-desktop: $*" >&2; }

for tool in curl sha256sum gpg gpgv dpkg-deb readelf glib-compile-schemas; do
    command -v "$tool" > /dev/null || { say "$tool is not installed"; exit 1; }
done

# Download $2/$1 into the pool as $4 (default: $1's last component) unless
# it is there, and check it against $3.
fetch() {
    local path=$1 from=$2 sum=$3 name=${4:-${1##*/}}
    local file="$out/pool/$name"
    if [ ! -f "$file" ]; then
        curl -fsSL -o "$file.part" "$from/$path"
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { say "$file does not match its checksum $sum"; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

mkdir -p "$out/pool"
rm -f "$out/claude-desktop.stamp"

# The repository's chain, checked afresh on every run into a directory of
# its own: what an earlier run left is not trusted.
apt="$out/apt"
rm -rf "$apt"
mkdir -p "$apt/gnupg"
chmod 700 "$apt/gnupg"
curl -fsSL -o "$apt/key.asc" "$key_url"
primary=$(GNUPGHOME="$apt/gnupg" gpg --batch --quiet --show-keys --with-colons "$apt/key.asc" \
    | awk -F: '$1 == "pub" { want = 1; next } want && $1 == "fpr" { print $10; want = 0 }')
[ "$primary" = "$KEY_FINGERPRINT" ] \
    || { say "the repository's key is ${primary:-none}, not $KEY_FINGERPRINT"; exit 1; }
GNUPGHOME="$apt/gnupg" gpg --batch --quiet --dearmor --output "$apt/key.gpg" "$apt/key.asc"

curl -fsSL -o "$apt/InRelease" "$repository/dists/stable/InRelease"
# gpgv writes the signed text alone to `Release`; only that is read below.
# VALIDSIG's last field is the fingerprint of the primary key that made it.
status=$(gpgv --keyring "$apt/key.gpg" --status-fd 1 --output "$apt/Release" \
    "$apt/InRelease" 2> "$apt/gpgv.log") \
    || { say "gpgv refuses InRelease:"; cat "$apt/gpgv.log" >&2; exit 1; }
signer=$(printf '%s\n' "$status" | awk '$2 == "VALIDSIG" { print $NF }')
[ "$signer" = "$KEY_FINGERPRINT" ] \
    || { say "InRelease is not signed by $KEY_FINGERPRINT (VALIDSIG: ${signer:-none})"; exit 1; }

# The index's SHA-256 and size, from the SHA256 section of the signed text.
read -r index_sum index_size < <(awk '
    /^[^ ]/ { section = $1 }
    section == "SHA256:" && $3 == "main/binary-amd64/Packages" { print $1, $2 }' "$apt/Release") || true
[ -n "${index_sum:-}" ] || { say "InRelease lists no main/binary-amd64/Packages"; exit 1; }
curl -fsSL -o "$apt/Packages" "$repository/dists/stable/main/binary-amd64/Packages"
[ "$(stat -c %s "$apt/Packages")" = "$index_size" ] \
    && echo "$index_sum  $apt/Packages" | sha256sum -c --quiet \
    || { say "Packages does not match what InRelease lists for it"; exit 1; }

# The pinned version's stanza: its file and its SHA-256.
read -r deb_path deb_sum < <(awk -v version="$CLAUDE_DESKTOP_VERSION" '
    /^$/ { if (package == "claude-desktop" && found == version) print file, sum
           package = found = file = sum = "" }
    /^Package: / { package = $2 } /^Version: / { found = $2 }
    /^Filename: / { file = $2 } /^SHA256: / { sum = $2 }
    END { if (package == "claude-desktop" && found == version) print file, sum }' "$apt/Packages") || true
[ -n "${deb_path:-}" ] \
    || { say "Packages lists no claude-desktop $CLAUDE_DESKTOP_VERSION"; exit 1; }
[ "$deb_sum" = "$CLAUDE_DESKTOP_SHA256" ] \
    || { say "Packages lists $deb_sum for claude-desktop $CLAUDE_DESKTOP_VERSION, not the pinned $CLAUDE_DESKTOP_SHA256"; exit 1; }
package=$(fetch "$deb_path" "$repository" "$deb_sum")

tree="$out/tree"
rm -rf "$tree"
mkdir -p "$tree"
for entry in "${debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$debian/pool" "$sum")" "$tree"
done
# The package's files only: `-x` runs none of its maintainer scripts.
dpkg-deb -x "$package" "$tree"

# Documentation and manuals, which nothing runs; GTK's input-method table,
# which no input method here reads; and the sandbox helper, which is setuid
# root in the package and which `--no-sandbox` never starts.
rm -rf "$tree/usr/share/doc" "$tree/usr/share/man" "$tree/usr/share/lintian" \
    "$tree/etc" "$tree/usr/lib/claude-desktop/chrome-sandbox"

glib-compile-schemas "$tree/usr/share/glib-2.0/schemas"

app="$tree/usr/lib/claude-desktop"
test -x "$app/claude-desktop" || { say "no claude-desktop in the tree"; exit 1; }
[ "$(cat "$app/version" 2> /dev/null)" != "" ] || { say "no Electron version in the package"; exit 1; }
test -f "$tree/usr/share/glib-2.0/schemas/gschemas.compiled" \
    || { say "GTK's schemas did not compile"; exit 1; }

# Every library an ELF file in the tree needs must be in it, beside the file
# itself (Electron's own libffmpeg and libvulkan), or in a base tree.
dirs=("$tree/usr/lib/x86_64-linux-gnu" "$tree/usr/lib64")
IFS=: read -r -a bases <<< "$base"
for each in "${bases[@]}"; do
    [ -d "$each" ] \
        || { say "$each is not there: the volume it is the tree of is merged before this one, fetch it first"; exit 1; }
    dirs+=("$each/usr/lib/x86_64-linux-gnu" "$each/usr/lib64")
done
missing=0
while IFS= read -r -d '' file; do
    head -c 4 "$file" | grep -q $'\x7fELF' || continue
    for needed in $(readelf -d "$file" 2> /dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
        found=0
        for dir in "${dirs[@]}" "$(dirname "$file")"; do
            [ -e "$dir/$needed" ] && { found=1; break; }
        done
        [ "$found" = 1 ] && continue
        case " $COWORK_ONLY " in
            *" $needed "*) [ "${file##*/}" = virtiofsd ] && continue ;;
        esac
        say "${file#"$tree"/} needs $needed, which is on no volume"
        missing=1
    done
done < <(find "$app" "$tree/usr/lib/x86_64-linux-gnu" -type f -print0)
[ "$missing" = 0 ] || exit 1

echo "$CLAUDE_DESKTOP_VERSION" > "$out/claude-desktop.stamp"
echo "claude-desktop $CLAUDE_DESKTOP_VERSION: $tree ($(du -sm "$tree" | cut -f1) MiB), for the --everything volume"
