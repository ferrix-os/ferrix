#!/usr/bin/env bash
# Build virglrenderer's test server with Ferrix's dmabuf import, for hyprix
# drawing its frames on the RTX 3060 (`--renderer vtest`, docs/NVIDIA.md §4.6)
# straight into a buffer the display engine scans out.
#
# virgl_test_server is virglrenderer 1.2.0, pinned by the SHA-256 of
# GitLab's tarball of the tag (commit below), with Ferrix's own patches,
# tools/common/data/virgl-server/*.patch in their order:
# 0001-vtest-ferrix-resource-import-fd.patch, the command
# VCMD_FERRIX_RESOURCE_IMPORT_FD (vtest_protocol.h, number 64) and the
# parameter VCMD_PARAM_FERRIX_IMPORT_FD a client asks for first; and
# 0002-vtest-ferrix-dmabuf-modifiers.patch, VCMD_FERRIX_DMABUF_MODIFIERS
# (65): the modifiers the server's EGL imports a format with, which is what
# hyprix offers its clients (the parameter's value is then 2). A stock
# server answers that parameter "not valid", so hyprix works with either;
# with the stock one its frames are read back (vtest protocol 2) and its
# clients' buffers are mapped.
#
# It is built as an x86-64 glibc program against Debian 13's -dev packages,
# unpacked into a sysroot here, the way fetch-yserver.sh builds yserver, so
# it runs on the Debian 13 libraries fetch-nvidia.sh puts on the NVIDIA
# volume (libepoxy0, libdrm2, libgbm1 and theirs). libvirglrenderer is
# linked into the program statically: nothing on the volume is replaced but
# the program, and Debian's libvirglrenderer1 is not loaded by it.
#
# virglrenderer is MIT-licensed (its COPYING, copied beside the program as
# virgl_test_server.COPYING); the patches are Ferrix's, under the same terms.
#
# Writes $FERRIX_VIRGL_SERVER/ (default ~/.local/share/ferrix/virgl-server):
# downloads/, sysroot/, src/, build/, and out/usr/bin/virgl_test_server with
# out/usr/share/doc/virgl-server/COPYING and out/virgl-server.version (the
# stamp: tarball, patches and script hashes). Needs curl, sha256sum, tar,
# patch, dpkg-deb, meson, ninja, gcc, python3 and readelf; no root.
#
# Usage: tools/common/fetch/fetch-virgl-server.sh

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
patch_dir=$here/../data/virgl-server
debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
out_dir=${FERRIX_VIRGL_SERVER:-$HOME/.local/share/ferrix/virgl-server}

VERSION=1.2.0
SOURCE_URL=https://gitlab.freedesktop.org/virgl/virglrenderer/-/archive/$VERSION/virglrenderer-$VERSION.tar.gz
SOURCE_SHA256=8bae92909021ea87e2087ae96df322f90140c414bd5f38810af3f3e5146a567b
# The commit the tag names (2025-09-08), for the record.
SOURCE_COMMIT=500b41d5c8638f9b80dd558f4044f3301c7457a4

# Pool path and SHA-256 of each package of the sysroot, from trixie's
# Packages file on 2026-10-10. The runtime halves are the ones
# fetch-nvidia.sh puts on the volume.
debs=(
    "pool/main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb 967aa62605721081c3eb2a17650611a792aa802d76a6511d1840242623d204c9"
    "pool/main/g/glibc/libc6-dev_2.41-12+deb13u4_amd64.deb 1fda734dabcd80b77266a09ab62b0f1e3e16d8091db62899890b2200745632a2"
    "pool/main/l/linux/linux-libc-dev_6.12.107-1_all.deb dc68dd53c7356598d005866da0d19d7bd33af2a62971aced624cdf04492902b4"
    "pool/main/libe/libepoxy/libepoxy0_1.5.10-2_amd64.deb 4c4c8024f2175086de65bca9fdc3fbb967f2863ebf249d489c66d0c8103dd3a3"
    "pool/main/libe/libepoxy/libepoxy-dev_1.5.10-2_amd64.deb c53e04d0ef7348542b4e94ddc373a3791304e56b4a78cd72c72ec0cc86e40a44"
    "pool/main/libd/libdrm/libdrm2_2.4.124-2_amd64.deb fe2276901c7cd7b8079de63072d37fe1cbeb4eb001a3bc1f1d662ad89aa0890e"
    "pool/main/libd/libdrm/libdrm-dev_2.4.124-2_amd64.deb 83b84b1207c4dda3d64ebad7725a7ad237c69c870668db3b01eabad1052f0dcb"
    "pool/main/m/mesa/libgbm1_25.0.7-2+deb13u1_amd64.deb 31fb6d76b9ceaf13848fa617df53f85f62626b4fe7464a93811c720af6d5f2dd"
    "pool/main/m/mesa/libgbm-dev_25.0.7-2+deb13u1_amd64.deb 8e3f9958fbcd4838953ebc7cb11e661e26652059473278d8f7e130c246f97c2a"
    # What libgbm1 loads, so that the link finds every library it names.
    "pool/main/w/wayland/libwayland-server0_1.23.1-3_amd64.deb 2967212bd582e0dffca443fdc44f4c660e7368d41f7ee3a7f6314e0c3abfe9ea"
    "pool/main/e/expat/libexpat1_2.8.3-1~deb13u1_amd64.deb 38abe0e710a07688e9c149d74536e67cfee0364bdb64dd6d644c32a1cfad389f"
    "pool/main/libf/libffi/libffi8_3.4.8-2_amd64.deb 0ebdc340de33333639c3c63874cd4b15ac2e83dfa1ef3053b7eefaf4919f4f68"
    "pool/main/libg/libglvnd/libegl-dev_1.7.0-1+b2_amd64.deb 97fcfff08a5de0a510066d55260a30fbd3ccbd87da6c2f317e6efe17566b9f5a"
    "pool/main/libg/libglvnd/libgl-dev_1.7.0-1+b2_amd64.deb 2c5044a44bc7cd7a196d554ca6ff78ae0358fbb98b7d2cce49176bdc6a816fe4"
)

for tool in curl sha256sum tar patch dpkg-deb meson ninja gcc python3 readelf; do
    command -v "$tool" > /dev/null || { echo "fetch-virgl-server: needs $tool" >&2; exit 1; }
done
case $out_dir in
    "$(git -C "$here" rev-parse --show-toplevel 2> /dev/null || echo /nonexistent)"*)
        echo "fetch-virgl-server: will not write inside a git checkout" >&2
        exit 1
        ;;
esac

downloads=$out_dir/downloads
mkdir -p "$downloads"

# fetch URL FILE SHA256: download once, check every time.
fetch() {
    if [ ! -f "$2" ]; then
        curl -fL --retry 3 -o "$2.part" "$1"
        mv "$2.part" "$2"
    fi
    echo "$3  $2" | sha256sum -c --quiet \
        || { echo "fetch-virgl-server: $2 is not the pinned file" >&2; exit 1; }
}

tarball=$downloads/virglrenderer-$VERSION.tar.gz
fetch "$SOURCE_URL" "$tarball" "$SOURCE_SHA256"
deb_files=()
for entry in "${debs[@]}"; do
    read -r path sum <<< "$entry"
    file=$downloads/$(basename "$path")
    fetch "$debian/$path" "$file" "$sum"
    deb_files+=("$file")
done

patch_files=("$patch_dir"/[0-9]*.patch)
[ -f "${patch_files[0]}" ] || { echo "fetch-virgl-server: no patches in $patch_dir" >&2; exit 1; }
stamp="$VERSION source=$SOURCE_SHA256 patch=$(cat "${patch_files[@]}" | sha256sum | cut -d' ' -f1) debs=$(printf '%s\n' "${debs[@]}" | sha256sum | cut -d' ' -f1) script=$(sha256sum < "$0" | cut -d' ' -f1)"
result=$out_dir/out
if [ -x "$result/usr/bin/virgl_test_server" ] \
    && [ "$(cat "$result/virgl-server.version" 2> /dev/null)" = "$stamp" ]; then
    echo "fetch-virgl-server: up to date: $result/usr/bin/virgl_test_server"
    exit 0
fi

# The sysroot. Debian 13 is merged-/usr; glibc's linker scripts name
# /lib/x86_64-linux-gnu/..., found inside the sysroot through these links.
sysroot=$out_dir/sysroot
rm -rf "$sysroot"
for merged in lib lib64 bin sbin; do
    mkdir -p "$sysroot/usr/$merged"
    ln -s "usr/$merged" "$sysroot/$merged"
done
for file in "${deb_files[@]}"; do
    dpkg-deb -x "$file" "$sysroot"
done
# libepoxy's pkg-config file asks for X11's, which an EGL-only build never
# includes (-Dplatforms=egl): it is told so, rather than the sysroot being
# given libX11's -dev chain for nothing.
sed -i -e 's/^Requires.private: x11, /Requires.private: /' -e 's/^epoxy_has_glx=1/epoxy_has_glx=0/' \
    "$sysroot/usr/lib/x86_64-linux-gnu/pkgconfig/epoxy.pc"

src=$out_dir/src
rm -rf "$src"
mkdir -p "$src"
tar xzf "$tarball" -C "$src" --strip-components=1
for patch_file in "${patch_files[@]}"; do
    patch -d "$src" -p1 --quiet < "$patch_file"
done

# Meson as a cross build, so that the compiler, the linker and pkg-config
# look only into the sysroot and never at the host's own libraries.
cross=$out_dir/cross.ini
cat > "$cross" << EOF
[binaries]
c = ['gcc', '--sysroot=$sysroot']
ar = 'ar'
strip = 'strip'
pkg-config = 'pkg-config'

[properties]
sys_root = '$sysroot'
pkg_config_libdir = ['$sysroot/usr/lib/x86_64-linux-gnu/pkgconfig', '$sysroot/usr/share/pkgconfig']
needs_exe_wrapper = true

[host_machine]
system = 'linux'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
EOF

build=$out_dir/build
rm -rf "$build"
PKG_CONFIG_SYSROOT_DIR=$sysroot meson setup "$build" "$src" --cross-file "$cross" \
    --buildtype=release --default-library=static -Db_ndebug=true \
    -Dplatforms=egl -Dminigbm_allocation=false -Dvenus=false -Ddrm-renderers=[] \
    -Drender-server=false -Dvideo=false -Dtests=false -Dfuzzer=false -Dvalgrind=false \
    -Dtracing=none > "$out_dir/meson-setup.log" \
    || { tail -40 "$out_dir/meson-setup.log" >&2; exit 1; }
ninja -C "$build" vtest/virgl_test_server > "$out_dir/ninja.log" \
    || { tail -40 "$out_dir/ninja.log" >&2; exit 1; }

program=$build/vtest/virgl_test_server
# What it loads must be what the volume has: Debian 13's, by soname.
needed=$(readelf -d "$program" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | sort | tr '\n' ' ')
for lib in $needed; do
    [ -e "$sysroot/usr/lib/x86_64-linux-gnu/$lib" ] \
        || { echo "fetch-virgl-server: the program needs $lib, which the sysroot lacks" >&2; exit 1; }
done
case " $needed " in
    *libvirglrenderer*) echo "fetch-virgl-server: libvirglrenderer is not linked in" >&2; exit 1 ;;
esac
# No symbol from a glibc newer than Debian 13's 2.41.
if readelf -V "$program" | grep -oE 'GLIBC_2\.[0-9]+' | sort -uV | tail -1 \
    | awk -F. '{ exit !($2 > 41) }'; then
    echo "fetch-virgl-server: the program asks for a glibc newer than 2.41" >&2
    exit 1
fi

rm -rf "$result"
mkdir -p "$result/usr/bin" "$result/usr/share/doc/virgl-server"
install -m 755 "$program" "$result/usr/bin/virgl_test_server"
strip "$result/usr/bin/virgl_test_server"
install -m 644 "$src/COPYING" "$result/usr/share/doc/virgl-server/COPYING"
install -m 644 "${patch_files[@]}" "$result/usr/share/doc/virgl-server/"
echo "$stamp" > "$result/virgl-server.version"
echo "fetch-virgl-server: $result/usr/bin/virgl_test_server"
echo "            virglrenderer $VERSION, sha256 $SOURCE_SHA256, commit $SOURCE_COMMIT"
echo "            needs: $needed"
