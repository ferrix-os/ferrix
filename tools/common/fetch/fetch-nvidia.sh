#!/usr/bin/env bash
# Fetch NVIDIA's driver at the release Ferrix pins, build its two OS-agnostic
# cores as freestanding objects, and put its userspace and GSP firmware on a
# btrfs volume (docs/NVIDIA.md §3, slice N1a).
#
# Four things, each skipped when it is already there and checks out:
#
#  1. The sources: open-gpu-kernel-modules at the 580.173.02 tag, as GitHub's
#     tarball of it, pinned by its SHA-256 and by the commit its pax header
#     names, unpacked into src/. `FERRIX_NVIDIA_SRC` points
#     `cargo xtask test-uvm` and uvm-kpi's Makefile at another tree; by
#     default they use this one.
#  2. The objects: nv-kernel.o (RM) and nv-modeset-kernel.o (NVKMS), built by
#     `make` in src/nvidia and src/nvidia-modeset with NVIDIA's own makefiles,
#     unmodified, and the host's gcc, as the feasibility pass built them
#     (§2.2). They are copied to objects/ with the lists of what each defines
#     and leaves undefined, and the summary compares the counts with §2.2's.
#  3. The userspace: NVIDIA-Linux-x86_64-580.173.02.run, pinned by the
#     SHA-256 NVIDIA publishes beside it, extracted with --extract-only into
#     run/. Nothing is installed: the .run's installer never runs.
#  4. The volume: nvidia.img, a btrfs image of tree/, Debian-shaped like
#     Chrome's and yserver's, which Ferrix mounts at /data:
#       usr/lib/x86_64-linux-gnu/  the x86-64 libraries and their links,
#                                  as NVIDIA's installer names them, and
#                                  Debian 13's glibc (the pin
#                                  fetch-chrome.sh uses), so nvidia-smi
#                                  runs from this volume alone
#       usr/lib64/                 glibc's loader
#       usr/bin/                   nvidia-smi, nvidia-debugdump, the CUDA
#                                  MPS pair, nvidia-persistenced
#       usr/share/{vulkan,glvnd,egl,nvidia}, etc/OpenCL/vendors
#                                  the ICD, layer and platform manifests
#       lib/firmware/nvidia/580.173.02/gsp_{ga10x,tu10x}.bin
#                                  the GSP firmware, where nvrm reads it
#     It does not carry usr/lib/ferrix/nvrm-core, RM's core nvrm loads
#     (docs/NVIDIA.md §4.1, "The core"): the core is pinned to one nvrm
#     build, so xtask writes it into the volume beside nvrm's own build
#     and remakes the image when it is stale (run-nvidia, N1d).
#       usr/share/doc/nvidia-driver/LICENSE
#     The 32-bit libraries, the X driver, nvidia-settings and its GTK
#     libraries, the installer and the Windows DLLs are left out.
#     nvidia.version beside the image says what it was made from;
#     it is made again when that changes, or this script does.
#
# Licences. The RM and NVKMS cores in src/ are MIT, file by file. The .run's
# userspace and its firmware are under the NVIDIA Driver License Agreement
# (run/LICENSE, and on the volume): proprietary, redistributable only
# unmodified. They are fetched by whoever runs this and never committed:
# everything this script writes is under $FERRIX_NVIDIA (default
# ~/.local/share/ferrix/nvidia), and it refuses to write inside a git
# checkout.
#
# Where nazuna's own driver is the same release, the extracted nvidia-smi,
# libraries and firmware are compared with the installed ones, read only.
#
# Writes $FERRIX_NVIDIA/580.173.02/: downloads/, src/, objects/, run/, tree/,
# nvidia.img and nvidia.version. Needs curl, sha256sum, git, tar, make, gcc,
# nm, size, readelf, dpkg-deb and mkfs.btrfs; no root, and it writes nothing
# outside that directory.
#
# Usage: tools/common/fetch/fetch-nvidia.sh

set -euo pipefail

VERSION=580.173.02

# GitHub's tarball of the tag, and the commit the tag names (2026-10-03).
SOURCES_URL=https://github.com/NVIDIA/open-gpu-kernel-modules/archive/refs/tags/$VERSION.tar.gz
SOURCES_SHA256=a2cd41cf100a81d90de9d5ca192b828ed8a63408330acef7931df00487acd82f
SOURCES_COMMIT=20e4e6e19cc26ba47b5cbe23130a396be100c427

# The .run, as NVIDIA's own .sha256sum beside it gives it (2026-10-03).
RUN_NAME=NVIDIA-Linux-x86_64-$VERSION.run
RUN_URL=${NVIDIA_DOWNLOAD:-https://us.download.nvidia.com/XFree86/Linux-x86_64}/$VERSION/$RUN_NAME
RUN_SHA256=8d8eb9001e05a9a8a663d3d5d304feb64ef2844ee185ccdfd952786820f46e1b

# What the .run must hold, checked after extraction. nazuna's installed
# 580.173.02 (Ubuntu's nvidia-utils-580 and nvidia-firmware-580) has the
# same bytes.
contents=(
    "firmware/gsp_ga10x.bin 32cb7cf3e77f97e92f6e92135939ccb3ba382748144569c0dfccf3338d43d963"
    "firmware/gsp_tu10x.bin 6f3ccbd570c7ac2a7ea910d9d87fc3d23db9ae3dfe82020ea07b17a30954495e"
    "nvidia-smi 22964713c1701fb62b4dd10b26b0dd25d174e100af5bda20c65e0b0fcc32b3be"
)

# Debian 13's glibc, the build fetch-chrome.sh pins, so the volume merges
# with Chrome's and yserver's without two copies of one file.
debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
LIBC_DEB="pool/main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb 967aa62605721081c3eb2a17650611a792aa802d76a6511d1840242623d204c9"

# §2.2's figures for the two objects, from the feasibility pass.
expected=(
    "nv-kernel.o 12169725 13372 406"
    "nv-modeset-kernel.o 1502689 2694 69"
)

root=${FERRIX_NVIDIA:-$HOME/.local/share/ferrix/nvidia}
out=$root/$VERSION

for tool in curl sha256sum git tar make gcc nm size readelf dpkg-deb mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-nvidia: $tool is not installed" >&2; exit 1; }
done

mkdir -p "$out/downloads"
# NVIDIA's userspace must never land in a repository.
if git -C "$out" rev-parse --show-toplevel > /dev/null 2>&1; then
    echo "fetch-nvidia: $out is inside a git checkout; set FERRIX_NVIDIA to a directory outside it" >&2
    exit 1
fi

# Download $1 to $2 unless it is there, and check it against $3.
fetch() {
    local url=$1 file=$2 sum=$3
    if [ ! -f "$file" ]; then
        echo "fetch-nvidia: downloading $url"
        curl -fsSL -o "$file.part" "$url"
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { echo "fetch-nvidia: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
}

# 1. The sources.
tarball=$out/downloads/open-gpu-kernel-modules-$VERSION.tar.gz
fetch "$SOURCES_URL" "$tarball" "$SOURCES_SHA256"
# git reads only the header, so gzip may die of SIGPIPE.
commit=$({ gzip -dc "$tarball" 2> /dev/null || true; } | git get-tar-commit-id)
[ "$commit" = "$SOURCES_COMMIT" ] \
    || { echo "fetch-nvidia: $tarball is commit $commit, not $SOURCES_COMMIT" >&2; exit 1; }
src=$out/src
if [ "$(cat "$src/.ferrix-sources" 2> /dev/null)" != "$SOURCES_SHA256" ]; then
    rm -rf "$src" "$src.new"
    mkdir -p "$src.new"
    tar -xzf "$tarball" -C "$src.new" --strip-components=1
    echo "$SOURCES_SHA256" > "$src.new/.ferrix-sources"
    mv "$src.new" "$src"
fi

# 2. The objects, with NVIDIA's makefiles; make rebuilds only what changed.
jobs=$(nproc)
[ "$jobs" -gt 8 ] && jobs=8
for part in nvidia nvidia-modeset; do
    echo "fetch-nvidia: building src/$part"
    make -s -C "$src/src/$part" -j"$jobs" > "$out/build-$part.log" 2>&1 \
        || { echo "fetch-nvidia: make in src/$part failed; see $out/build-$part.log" >&2; exit 1; }
done
objects=$out/objects
mkdir -p "$objects"
cp "$src/src/nvidia/_out/Linux_x86_64/nv-kernel.o" "$objects/"
cp "$src/src/nvidia-modeset/_out/Linux_x86_64/nv-modeset-kernel.o" "$objects/"
for object in nv-kernel.o nv-modeset-kernel.o; do
    nm -u "$objects/$object" | awk '{print $2}' | sort > "$objects/$object.undef"
    nm -g --defined-only "$objects/$object" | awk '{print $3}' | sort > "$objects/$object.defined"
done

# 3. The userspace, extracted and never installed.
installer=$out/downloads/$RUN_NAME
fetch "$RUN_URL" "$installer" "$RUN_SHA256"
run=$out/run
if [ "$(cat "$run/.ferrix-run" 2> /dev/null)" != "$RUN_SHA256" ]; then
    rm -rf "$run" "$run.new"
    sh "$installer" --extract-only --target "$run.new" > /dev/null
    echo "$RUN_SHA256" > "$run.new/.ferrix-run"
    mv "$run.new" "$run"
fi
for entry in "${contents[@]}"; do
    read -r path sum <<< "$entry"
    echo "$sum  $run/$path" | sha256sum -c --quiet \
        || { echo "fetch-nvidia: $run/$path does not match its pinned checksum" >&2; exit 1; }
done
read -r libc_path libc_sha256 <<< "$LIBC_DEB"
libc_deb=$out/downloads/${libc_path##*/}
fetch "$debian/$libc_path" "$libc_deb" "$libc_sha256"

# 4. The volume, made again when what it is made from, or this script, has
# changed.
image=$out/nvidia.img
stamp="$VERSION run=$RUN_SHA256 libc=$libc_sha256 script=$(sha256sum < "$0" | cut -d' ' -f1)"
if [ ! -f "$image" ] || [ "$(cat "$out/nvidia.version" 2> /dev/null)" != "$stamp" ]; then
    tree=$out/tree
    rm -rf "$tree" "$image" "$out/nvidia.version"
    lib=$tree/usr/lib/x86_64-linux-gnu
    mkdir -p "$lib" "$tree/usr/lib64" "$tree/usr/bin"
    dpkg-deb -x "$libc_deb" "$tree"

    # The x86-64 libraries and the links NVIDIA's installer would make, from
    # the .run's manifest: `file mode TYPE NATIVE [dir/] [target] MODULE:m`.
    # Hard links into run/, which is never changed after extraction.
    while read -r kind name dir target; do
        [ "$dir" = "-" ] && dir=""
        mkdir -p "$lib/$dir"
        case $kind in
            file) ln -f "$run/$name" "$lib/$dir$name" ;;
            link) ln -sfn "$target" "$lib/$dir$name" ;;
        esac
    done < <(awk '
        NR > 8 && $4 == "NATIVE" && $NF !~ /^MODULE:(installer|xdriver|xutils)$/ {
            dir = ($5 ~ /\/$/ && $5 != "/") ? $5 : "-"
            if ($3 ~ /^(OPENGL|CUDA|UTILITY|TLS|GLVND|GLX_CLIENT|EGL_CLIENT|OPENCL|OPENCL_WRAPPER|NVCUVID|ENCODEAPI|VDPAU)_LIB$/)
                print "file", $1, dir, "-"
            else if ($3 == "GBM_BACKEND_LIB_SYMLINK")
                print "link", $1, "gbm/", "../" $(NF - 1)
            else if ($3 ~ /_SYMLINK$/ && $3 !~ /^(SYSTEMD_UNIT|UTILITY_BIN|GLX_MODULE)_SYMLINK$/)
                print "link", $1, dir, $(NF - 1)
        }' "$run/.manifest")
    # And each library's soname, as ldconfig would make it.
    for file in "$lib"/*.so.* "$lib"/vdpau/*.so.*; do
        [ -f "$file" ] && [ ! -L "$file" ] || continue
        soname=$(readelf -d "$file" | sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p')
        if [ -n "$soname" ] && [ ! -e "$(dirname "$file")/$soname" ]; then
            ln -s "$(basename "$file")" "$(dirname "$file")/$soname"
        fi
    done

    for program in nvidia-smi nvidia-debugdump nvidia-cuda-mps-control nvidia-cuda-mps-server \
        nvidia-persistenced; do
        ln -f "$run/$program" "$tree/usr/bin/$program"
    done
    mkdir -p "$tree/usr/share/vulkan/icd.d" "$tree/usr/share/vulkan/implicit_layer.d" \
        "$tree/usr/share/glvnd/egl_vendor.d" "$tree/usr/share/egl/egl_external_platform.d" \
        "$tree/usr/share/nvidia" "$tree/etc/OpenCL/vendors" "$tree/usr/share/doc/nvidia-driver" \
        "$tree/lib/firmware/nvidia/$VERSION"
    ln -f "$run/nvidia_icd.json" "$tree/usr/share/vulkan/icd.d/nvidia_icd.json"
    ln -f "$run/nvidia_layers.json" "$tree/usr/share/vulkan/implicit_layer.d/nvidia_layers.json"
    ln -f "$run/10_nvidia.json" "$tree/usr/share/glvnd/egl_vendor.d/10_nvidia.json"
    for platform in 10_nvidia_wayland 15_nvidia_gbm 20_nvidia_xcb 20_nvidia_xlib; do
        ln -f "$run/$platform.json" "$tree/usr/share/egl/egl_external_platform.d/$platform.json"
    done
    ln -f "$run/nvidia.icd" "$tree/etc/OpenCL/vendors/nvidia.icd"
    for data in nvidia-application-profiles-$VERSION-rc \
        nvidia-application-profiles-$VERSION-key-documentation nvoptix.bin; do
        ln -f "$run/$data" "$tree/usr/share/nvidia/$data"
    done
    ln -f "$run/LICENSE" "$tree/usr/share/doc/nvidia-driver/LICENSE"
    ln -f "$run/supported-gpus/supported-gpus.json" "$tree/usr/share/doc/nvidia-driver/supported-gpus.json"
    for firmware in gsp_ga10x.bin gsp_tu10x.bin; do
        ln -f "$run/firmware/$firmware" "$tree/lib/firmware/nvidia/$VERSION/$firmware"
    done

    # Everything nvidia-smi and the CUDA and NVML libraries load must be on
    # the volume itself; the GL, EGL and Vulkan libraries also load X11 and
    # Wayland client libraries, which the volumes they are used beside have.
    test -e "$tree/usr/lib64/ld-linux-x86-64.so.2" \
        || { echo "fetch-nvidia: no loader at usr/lib64" >&2; exit 1; }
    missing=0
    for file in "$tree"/usr/bin/* "$lib/libnvidia-ml.so.$VERSION" "$lib/libcuda.so.$VERSION"; do
        for needed in $(readelf -d "$file" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
            if [ ! -e "$lib/$needed" ]; then
                echo "fetch-nvidia: ${file#"$tree"/} needs $needed, which is not on the volume" >&2
                missing=1
            fi
        done
    done
    for link in "$lib"/*.so* "$lib"/*/*.so*; do
        [ -e "$link" ] || { echo "fetch-nvidia: ${link#"$tree"/} is a broken link" >&2; missing=1; }
    done
    [ "$missing" = 0 ] || exit 1

    # Room for nvidia-smi's and the libraries' caches.
    size=$(( $(du -sm "$tree" | cut -f1) + 128 ))
    truncate -s "${size}M" "$image"
    mkfs.btrfs -q --rootdir "$tree" "$image"
    echo "$stamp" > "$out/nvidia.version"
fi

# The summary.
echo
echo "NVIDIA $VERSION in $out"
echo "  sources   $SOURCES_URL"
echo "            sha256 $SOURCES_SHA256, commit $SOURCES_COMMIT"
echo "            unpacked in $src"
echo "  .run      $RUN_URL"
echo "            sha256 $RUN_SHA256"
echo "            extracted in $run"
echo "  compiler  $(gcc --version | head -n 1)"
echo "  objects   in $objects (§2.2's figures in brackets)"
for entry in "${expected[@]}"; do
    read -r object text defined undefined <<< "$entry"
    got_text=$(size "$objects/$object" | awk 'NR == 2 {print $1}')
    got_defined=$(wc -l < "$objects/$object.defined")
    got_undefined=$(wc -l < "$objects/$object.undef")
    note=""
    [ "$got_text $got_defined $got_undefined" = "$text $defined $undefined" ] || note="  DIFFERS from §2.2"
    printf '    %-20s text %9s [%s], %5s defined [%s], %3s undefined [%s]%s\n' \
        "$object" "$got_text" "$text" "$got_defined" "$defined" "$got_undefined" "$undefined" "$note"
done
# How many of an object's imports match an extended regular expression.
count() { grep -cE "$2" "$objects/$1.undef" || true; }
echo "    nv-kernel.o imports: $(count nv-kernel.o '^os_') os_* [150], $(count nv-kernel.o '^nv_') nv_* [117]," \
    "$(count nv-kernel.o '^libspdm_') libspdm_* [71], $(count nv-kernel.o '^(nvswitch|nvlink)_') nvswitch_*/nvlink_* [53]," \
    "$(count nv-kernel.o '^__x86_indirect_thunk') retpoline thunks [13]"
echo "    nv-modeset-kernel.o imports: $(count nv-modeset-kernel.o '^nvkms_') nvkms_* [55], plus memcpy and the 13 thunks"
host=identical
for pair in "nvidia-smi /usr/bin/nvidia-smi" \
    "libnvidia-ml.so.$VERSION /usr/lib/x86_64-linux-gnu/libnvidia-ml.so.$VERSION" \
    "libcuda.so.$VERSION /usr/lib/x86_64-linux-gnu/libcuda.so.$VERSION" \
    "firmware/gsp_ga10x.bin /lib/firmware/nvidia/$VERSION/gsp_ga10x.bin"; do
    read -r ours theirs <<< "$pair"
    if [ ! -r "$theirs" ]; then
        host="not installed here"
    elif ! cmp -s "$run/$ours" "$theirs"; then
        host="DIFFERENT: $theirs"
        break
    fi
done
echo "  host      nvidia-smi, libnvidia-ml, libcuda and gsp_ga10x.bin against this host's $VERSION: $host"
echo "  volume    $image ($(( $(stat -c %s "$image") / 1048576 )) MiB, btrfs), unpacked in $out/tree"
echo "            firmware lib/firmware/nvidia/$VERSION/gsp_ga10x.bin" \
    "($(stat -c %s "$run/firmware/gsp_ga10x.bin") bytes)"
