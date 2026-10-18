#!/usr/bin/env bash
# The Intel GPU's box checkpoint (docs/INTEL-GPU.md §6, M3): run Mesa's
# anv (`vulkaninfo --summary`) and iris (`eglinfo -B`) on the N150 box's
# running Ferrix, and keep what they say.
#
# Run from a machine that reaches both the box over ssh (as root, the box's
# default) and nazuna (`nazuna-wg`), where fetch-intel-uapi.sh's tree is:
#
#   tools/common/n150/intel-checkpoint.sh [BOX] [BOX_SSH_OPTIONS...]
#
# BOX defaults to 192.168.178.146, the box's address from the customer's
# Windows PC. Extra arguments go to ssh, for a known-hosts file or a key.
#
# What it does, and nothing else:
# 1. copies the volume tree of tools/common/fetch/fetch-intel-uapi.sh (Debian
#    13's Mesa 25.0.7 with anv and iris, glibc, vulkaninfo, eglinfo) from
#    nazuna to the box's /tmp/intel-uapi, unless that is there already;
# 2. on the box: lists /dev/dri and the render node's sysfs identity, the
#    kernel's lines about the Intel GPU, chardev and the IOMMU, then runs
#    vulkaninfo with anv's manifest alone and eglinfo -B, each through the
#    tree's own loader;
# 3. writes everything to ~/.local/share/ferrix/n150-linux/evidence/
#    intel-checkpoint-<UTC time>.txt here, and says the file.
#
# It boots nothing, writes nothing on the box but /tmp/intel-uapi, and needs
# the box already running a Ferrix with `ferrix.efifb ferrix.intelgpu` (K4:
# the desktop, and with it the box's sshd, needs a card) and the Intel
# driver; main schedules box time. What a run shows depends on where M2 and
# K2 stand: before K2 translates 00:02.0's DMA the kernel refuses the render
# node (`chardev ... refused while its DMA is untranslated`), and anv then
# finds no Intel device, which is itself the checkpoint's answer.

set -euo pipefail

box=${1:-192.168.178.146}
shift || true
ssh_box=(ssh -o BatchMode=yes -o ConnectTimeout=10 "$@" "root@$box")
tree=/home/sebastian/.local/share/ferrix/intel-uapi/tree
there=/tmp/intel-uapi
evidence=$HOME/.local/share/ferrix/n150-linux/evidence
mkdir -p "$evidence"
out=$evidence/intel-checkpoint-$(date -u +%Y%m%dT%H%M%SZ).txt

"${ssh_box[@]}" true || { echo "intel-checkpoint: the box at $box does not answer ssh" >&2; exit 1; }

if ! "${ssh_box[@]}" "test -x $there/usr/bin/vulkaninfo"; then
    echo "intel-checkpoint: copying Mesa's Intel userspace from nazuna to the box's $there"
    ssh nazuna-wg "test -x $tree/usr/bin/vulkaninfo" \
        || { echo "intel-checkpoint: no tree on nazuna: run tools/common/fetch/fetch-intel-uapi.sh there" >&2; exit 1; }
    ssh nazuna-wg "tar -C $tree -cf - usr lib lib64" \
        | "${ssh_box[@]}" "mkdir -p $there && tar -C $there -xf -"
fi

# The run on the box, as a script on its stdin: its own loader and library
# path, so nothing of the box's /data is used, and anv's manifest written
# with its library's whole path.
script=$(cat << EOF
t=$there
lib=\$t/usr/lib/x86_64-linux-gnu
run() { "\$t/usr/lib64/ld-linux-x86-64.so.2" --library-path "\$lib" "\$@"; }
echo "== the box"
date -u
cat /proc/cmdline 2> /dev/null
echo "== /dev/dri"
ls -l /dev/dri 2>&1
for node in /dev/dri/renderD*; do
    [ -e "\$node" ] || continue
    minor=\${node#/dev/dri/renderD}
    for attr in vendor device revision subsystem_vendor subsystem_device class; do
        printf '%s %s: ' "\$node" "\$attr"; cat "/sys/dev/char/226:\$minor/device/\$attr" 2>&1
    done
done
echo "== the kernel's lines"
(dmesg 2> /dev/null || cat /proc/kmsg 2> /dev/null) | grep -i -E 'intel|chardev|iommu|00:02.0|renderD' | tail -n 80
echo "== vulkaninfo --summary (anv alone)"
printf '{"ICD":{"api_version":"1.4.305","library_path":"%s"},"file_format_version":"1.0.0"}\n' \
    "\$lib/libvulkan_intel.so" > /tmp/intel-checkpoint-icd.json
VK_ICD_FILENAMES=/tmp/intel-checkpoint-icd.json MESA_SHADER_CACHE_DISABLE=1 \
    run "\$t/usr/bin/vulkaninfo" --summary 2>&1
echo "== vulkaninfo exited \$?"
echo "== eglinfo -B"
__EGL_VENDOR_LIBRARY_FILENAMES=\$t/usr/share/glvnd/egl_vendor.d/50_mesa.json \
    LIBGL_DRIVERS_PATH=\$lib/dri MESA_SHADER_CACHE_DISABLE=1 \
    run "\$t/usr/bin/eglinfo.x86_64-linux-gnu" -B 2>&1
echo "== eglinfo exited \$?"
echo "== the driver's lines"
(dmesg 2> /dev/null || cat /proc/kmsg 2> /dev/null) | grep -E '^.*(intel: |intel-fake: )' | tail -n 40
EOF
)
{
    echo "intel-checkpoint: box $box, from $(hostname), $(date -u)"
    echo "intel-checkpoint: this checkout at $(git -C "$(dirname "$0")" rev-parse --short HEAD 2> /dev/null || echo '?')"
    printf '%s\n' "$script" | "${ssh_box[@]}" sh
} > "$out" 2>&1 || true
grep -E 'deviceName|driverID|OpenGL core profile renderer|exited|refused|unserved' "$out" || true
echo "intel-checkpoint: written to $out"
