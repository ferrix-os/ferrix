#!/usr/bin/env bash
# Make the volume `cargo xtask test-intel-uapi` attaches at /data: Debian 13's
# Mesa 25.0.7 with the Intel drivers kept -- anv (libvulkan_intel.so and its
# ICD manifest) and iris (in libgallium, through libEGL_mesa) -- glibc and
# what they load, the Vulkan loader, and the two programs the gate runs,
# vulkaninfo (vulkan-tools) and eglinfo (mesa-utils-bin)
# (docs/INTEL-GPU.md §6, M3).
#
# The packages are pinned by pool path and SHA-256, the same builds
# fetch-yserver.sh pins where both take a package, so the two volumes hold
# one Mesa. A package no longer on the mirror is fetched from
# snapshot.debian.org as it was the day the pins were taken. Every ELF file
# under usr/bin and the library directories must find each library it needs
# on the volume, or the script stops and names it.
#
# Writes $FERRIX_INTEL_UAPI (default ~/.local/share/ferrix/intel-uapi):
# pool/ (the .debs), tree/ (unpacked), intel-uapi.img (the btrfs volume)
# and intel-uapi.stamp, the volume's version below, which xtask compares
# with its own (`intel_uapi::VOLUME`) so that a volume an older script made
# is refused. Raise both together whenever a pin or the tree changes.
# Needs curl, sha256sum, dpkg-deb, readelf and mkfs.btrfs; never sudo.
#
# Usage: tools/common/fetch/fetch-intel-uapi.sh

set -euo pipefail

debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
security=${DEBIAN_SECURITY_MIRROR:-https://security.debian.org/debian-security}
snapshot=${DEBIAN_SNAPSHOT:-https://snapshot.debian.org/archive}
snapshot_time=20261018T235959Z
version=1
out=${FERRIX_INTEL_UAPI:-$HOME/.local/share/ferrix/intel-uapi}
pool=$out/pool

debs=(
    "pool/main/g/gcc-14/gcc-14-base_14.2.0-19_amd64.deb 5b6825de4263824b78c4c51f6476414f3b4e89c2ab63e81dc8b9b5501e867cf6"
    "pool/main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb 967aa62605721081c3eb2a17650611a792aa802d76a6511d1840242623d204c9"
    "pool/main/g/gcc-14/libgcc-s1_14.2.0-19_amd64.deb 3c71917b490d1a17aed43196a2787a256ecf060526cdb20216a74bedc061b150"
    "pool/main/g/gcc-14/libstdc++6_14.2.0-19_amd64.deb ab1fa05837aa7a92aae748fd07a18a35f7d18bb4a71c4724fe2bbf0e32089de0"
    "pool/main/v/vulkan-loader/libvulkan1_1.4.309.0-1_amd64.deb f47da79cd140264fe21cceb08bc87a71bc7fec05819e4a421f3f518d21101a37"
    "pool/main/v/vulkan-tools/vulkan-tools_1.4.304.0+dfsg1-1_amd64.deb 742d1cd78c333a94b8214e21ba455d47884dc32f110016ffe6998cf88f2a2f94"
    "pool/main/m/mesa/mesa-vulkan-drivers_25.0.7-2+deb13u1_amd64.deb caf2f7d296b7522efe74e40a74bce762a3ce84c33b53b5e54e1248ac2e0a13a5"
    "pool/main/m/mesa/mesa-libgallium_25.0.7-2+deb13u1_amd64.deb 3e610f29321cdcc61337c86e6b4031ff60f0c9915fe19e6a862ac06480620ff4"
    "pool/main/m/mesa/libgl1-mesa-dri_25.0.7-2+deb13u1_amd64.deb 722fad9944bdfd29eb6421576331a2697f83bc4b3551d693edb7aefdde6bb198"
    "pool/main/m/mesa/libgbm1_25.0.7-2+deb13u1_amd64.deb 31fb6d76b9ceaf13848fa617df53f85f62626b4fe7464a93811c720af6d5f2dd"
    "pool/main/libg/libglvnd/libegl1_1.7.0-1+b2_amd64.deb 62e7bc8ccd47a60e55aad17f83d8d474310d782eb8589bbce2ab714c31712406"
    "pool/main/m/mesa/libegl-mesa0_25.0.7-2+deb13u1_amd64.deb 0ddebd7e91b048aac812611d6bd008343386cb140b51e5e66df74c6b6263be65"
    "pool/main/libg/libglvnd/libgles2_1.7.0-1+b2_amd64.deb e12c18aa77939d122ee1dea520ba0440b97dc19200b7396e03dff03a3ece74c1"
    "pool/main/libg/libglvnd/libgl1_1.7.0-1+b2_amd64.deb 87fa2f6e5abaed4ed385fac879c8dd735af719ee2300222d901793c66e041678"
    "pool/main/libg/libglvnd/libglx0_1.7.0-1+b2_amd64.deb 2721fdca0fe3bd963cb39482eabc253af52b88f4a7f6dbb69e475549daf5af3b"
    "pool/main/libg/libglvnd/libglvnd0_1.7.0-1+b2_amd64.deb 887f74008166549ce9e100c906aa937e95d6e5ce1c8d86efe8c95fd953359b9c"
    "pool/main/m/mesa/libglx-mesa0_25.0.7-2+deb13u1_amd64.deb 2e6ab57de9931e890621d3ade04213b06e7142a4ae8a72a1756c44d0271aaeb7"
    "pool/main/m/mesa-demos/mesa-utils-bin_9.0.0-2+b2_amd64.deb 26ab6bf7ff781cb903a6b1b3518a1926a799024289853cc3693ee0dd922e1e13"
    "pool/main/libd/libdecor-0/libdecor-0-0_0.2.2-2_amd64.deb e4f228a9d4a38af23fab00f8e20219790b567e2e1f7804b3a215a26294350f82"
    "pool/main/w/wayland/libwayland-egl1_1.23.1-3_amd64.deb 7c5c3429028b39b9432792db08cb218c43891df439ebc648b83ebee676a80ec5"
    "pool/main/w/wayland/libwayland-cursor0_1.23.1-3_amd64.deb 26fccae2b1228744de0cad6c688550b8d13cd4e8d4ccf0cfdae015634ece673f"
    "pool/main/w/wayland/libwayland-client0_1.23.1-3_amd64.deb d1607e1db1a7c5378a2e5c6ae7b20f64dc6acf134f953d84a7f5204b74339f33"
    "pool/main/w/wayland/libwayland-server0_1.23.1-3_amd64.deb 2967212bd582e0dffca443fdc44f4c660e7368d41f7ee3a7f6314e0c3abfe9ea"
    "pool/main/libx/libxkbcommon/libxkbcommon0_1.7.0-2_amd64.deb f75ee544f55acc6a271debfab3ea4ae0458afc89d81cfe1a71137e07d4895b86"
    "pool/main/libx/libxkbcommon/libxkbcommon-x11-0_1.7.0-2_amd64.deb b5cff6287d0d90b66224edfb7208709c5d35511215d87d2836919e4f7d367653"
    "pool/main/libx/libxcb/libxcb-xkb1_1.17.0-2+b1_amd64.deb a46b8d860fbff32c8021dac6036d83859c24ad8700cd1b977cda116d37a2dd35"
    "pool/main/x/xkeyboard-config/xkb-data_2.42-1_all.deb 196ff18533382f64e057ea49df2bb486bd4275a4cc0917361edb560b8756dada"
    "pool/main/libd/libdrm/libdrm2_2.4.124-2_amd64.deb fe2276901c7cd7b8079de63072d37fe1cbeb4eb001a3bc1f1d662ad89aa0890e"
    "pool/main/libd/libdrm/libdrm-common_2.4.124-2_all.deb 9a8a6c65c165e9964f106fb4ac710959b5d33e0790227e3ab6b27c4742d1254a"
    "pool/main/libd/libdrm/libdrm-intel1_2.4.124-2_amd64.deb 188ab1fd74c838b3c8055d62107351e16a4bb7a25d39c30bbb9aac30ddd37238"
    "pool/main/libd/libdrm/libdrm-amdgpu1_2.4.124-2_amd64.deb c1d97a5e32e2bc68e833b8abeae026b6306198e87ee4396a7ffecd4779caefbf"
    "pool/main/libp/libpciaccess/libpciaccess0_0.17-3+b3_amd64.deb d9a0091071635a84e837051e4813005ac445071731becea28fba4d9806df5252"
    "pool/main/e/elfutils/libelf1t64_0.192-4_amd64.deb 94497b7e17b6f574a0605b380d454e20d3f01a9c63b70c2a2263f679d30053e1"
    "pool/main/l/llvm-toolchain-19/libllvm19_19.1.7-3+b1_amd64.deb db0d614d61345ca710fb73e75105f1ec6e38898fa7b3aa99fa7eb7d8b0a11d57"
    "pool/main/l/lm-sensors/libsensors5_3.6.2-2_amd64.deb f0a994a6d7cfa695dea5343d0d1ba7eed796c0ad920c7282998b95f60049c4f6"
    "pool/main/l/lm-sensors/libsensors-config_3.6.2-2_all.deb 3056da80c7d963af795dab480ab6f6f4b154ad4ac39f522dc52d17c834fea253"
    "pool/main/libe/libedit/libedit2_3.1-20250104-1_amd64.deb b002ea172b9c1e34a67bc497c523c67bb74c3f0a4e98113cb083990a1f1d3bfe"
    "pool/main/libb/libbsd/libbsd0_0.12.2-2_amd64.deb e5a85986fa6bec3307ab1bc860736b478b331882bc45e17675a7bdf88eecb43a"
    "pool/main/libm/libmd/libmd0_1.1.0-2+b1_amd64.deb 7244ec3839b61fac0c1884fe08aaa040f26e8f1f35f1f5d3482eacefd30d1b44"
    "pool/main/n/ncurses/libtinfo6_6.5+20250216-2_amd64.deb 8b9f6a7983e9418564e48a627518de4c03917b56efe68d7f3e93bd8fffa1cc10"
    "pool/main/libx/libxml2/libxml2_2.12.7+dfsg+really2.9.14-2.1+deb13u3_amd64.deb e0c6b63ce4602a036a526f60fe5e6c1586710688058d98fc1001b9b3147b7efd"
    "pool/main/z/z3/libz3-4_4.13.3-1_amd64.deb 71383373523ef62d47eccf660cf6535c3febcbb3f88e54cb7a43124014b57359"
    "pool/main/x/xz-utils/liblzma5_5.8.1-1+deb13u1_amd64.deb 1cfcc6e0dc36f438a79b6e2189facdb9d150b08f57d190a60e01c98075c7f896"
    "pool/main/libz/libzstd/libzstd1_1.5.7+dfsg-1_amd64.deb 2f6a2aeacfc925eba8b00ac9139bc4bfccf8cacb09eb93de067074b26948eef9"
    "pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_amd64.deb 015be740d6236ad114582dea500c1d907f29e16d6db00566ca32fb68d71ac90d"
    "pool/main/libx/libx11/libx11-6_1.8.12-1_amd64.deb b5a3fd3bf8c8fd0364bfb9bea00dcba7fc301229bd02dded084632d31f5b0fb3"
    "pool/main/libx/libx11/libx11-data_1.8.12-1_all.deb c54f87069888f80ba4da586da6147d74c7598ccdd8b90906dbc4271fa414c738"
    "pool/main/libx/libx11/libx11-xcb1_1.8.12-1_amd64.deb e05f94d21a932fba5b09b9b13d99df776b155d3bc792c0e294451df9ffe1ba25"
    "pool/main/libx/libxau/libxau6_1.0.11-1_amd64.deb 689a9f0e0ba3e2c65431f864871e303ee904de69dd28abfc462663fae030227f"
    "pool/main/libx/libxdmcp/libxdmcp6_1.1.5-1_amd64.deb 0740dc760916b2008b45417a42a8fd7dd5de370fb57d31373f15034cda8acf0b"
    "pool/main/libx/libxcb/libxcb1_1.17.0-2+b1_amd64.deb 5c222a72d11b866447da31693254f738430726e3e065a384e82687b2fd2f978b"
    "pool/main/libx/libxcb/libxcb-dri3-0_1.17.0-2+b1_amd64.deb f446d42fb5fcebbb3e347368ba83616769fdb271d85b2f49048e337f3163d267"
    "pool/main/libx/libxcb/libxcb-present0_1.17.0-2+b1_amd64.deb db95ea4630c55bd7f3281cb60ccf75b1627ef2f4399e1939f8f6161e584a92fb"
    "pool/main/libx/libxcb/libxcb-randr0_1.17.0-2+b1_amd64.deb f5d9fe5fdf797918f81f0abf6cfd4270bd54659540cddb4da709ab7154523214"
    "pool/main/libx/libxcb/libxcb-shm0_1.17.0-2+b1_amd64.deb d1777a813f484e89f3dcaadd1d5f2b1a13e9313b6bf70eded16d8ce743208fef"
    "pool/main/libx/libxcb/libxcb-sync1_1.17.0-2+b1_amd64.deb 0ce4770ed1505be9ddc6473045e4662d062aff2f3077ee684265f01cfb543559"
    "pool/main/libx/libxcb/libxcb-xfixes0_1.17.0-2+b1_amd64.deb f9c1aafc18bf9e4662e34bbaf3bb00f1f4e8c2fc1dd786fdfb6cb9cb6c64fae7"
    "pool/main/libx/libxcb/libxcb-glx0_1.17.0-2+b1_amd64.deb cc59a3fa1c4ce376c3a32798179950810bd7b9be9a35bc76de1f5b5e7cc3925e"
    "pool/main/libx/libxshmfence/libxshmfence1_1.3.3-1_amd64.deb 7b339e9e5b2349723d35af4df89bcc7aa456bbdf8ba1754358f9b44c3fe1f964"
    "pool/main/libx/libxext/libxext6_1.3.4-1+b3_amd64.deb fc618ec40465e5ce48622606299cb47833efc3fb235ba15543b81f850722f443"
    "pool/main/libx/libxxf86vm/libxxf86vm1_1.1.4-1+b4_amd64.deb f9f8487e536293e7ecf91fe7feb14e97e02a3aa652e207c661442dce775d3b14"
    "pool/main/libf/libffi/libffi8_3.4.8-2_amd64.deb 0ebdc340de33333639c3c63874cd4b15ac2e83dfa1ef3053b7eefaf4919f4f68"
    "pool/updates/main/e/expat/libexpat1_2.8.3-1~deb13u1_amd64.deb 38abe0e710a07688e9c149d74536e67cfee0364bdb64dd6d644c32a1cfad389f"
)

for tool in curl sha256sum dpkg-deb readelf mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-intel-uapi: $tool is not installed" >&2; exit 1; }
done

# Download $1 into the pool unless it is there, from the snapshot when the
# mirror no longer has it, and check it against $2.
fetch() {
    local path=$1 sum=$2 base=$debian archive=debian
    if [[ $path == pool/updates/* ]]; then
        base=$security
        archive=debian-security
    fi
    local file="$pool/${path##*/}"
    if [ ! -f "$file" ]; then
        if ! curl -fsSL -o "$file.part" "$base/$path"; then
            echo "fetch-intel-uapi: ${path##*/} is not at $base; from $snapshot/$archive/$snapshot_time" >&2
            curl -fsSL -o "$file.part" "$snapshot/$archive/$snapshot_time/$path"
        fi
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { echo "fetch-intel-uapi: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

mkdir -p "$pool"
tree=$out/tree
rm -rf "$tree"
mkdir -p "$tree"
# Debian 13's merged /usr: the loader at /lib64, the libraries at
# /lib/x86_64-linux-gnu, both found through these links on the volume as
# the image's own links (xtask's `intel_uapi::LINKS`) find them at /.
for merged in bin lib lib64 sbin; do
    mkdir -p "$tree/usr/$merged"
    ln -s "usr/$merged" "$tree/$merged"
done
for entry in "${debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$sum")" "$tree"
done
rm -rf "$tree/usr/share/doc" "$tree/usr/share/man" "$tree/usr/share/locale" \
    "$tree/usr/share/lintian" "$tree/usr/share/bug"

lib=$tree/usr/lib/x86_64-linux-gnu
for wanted in usr/bin/vulkaninfo usr/bin/eglinfo usr/lib/x86_64-linux-gnu/libvulkan_intel.so \
    usr/share/vulkan/icd.d/intel_icd.x86_64.json usr/lib/x86_64-linux-gnu/libEGL_mesa.so.0 \
    usr/lib/x86_64-linux-gnu/dri/iris_dri.so usr/share/glvnd/egl_vendor.d/50_mesa.json; do
    test -e "$tree/$wanted" || { echo "fetch-intel-uapi: no $wanted on the volume" >&2; exit 1; }
done

# Every library an ELF file on the volume needs must be on it.
missing=0
while IFS= read -r -d '' file; do
    head -c 4 "$file" | grep -q $'\x7fELF' || continue
    for needed in $(readelf -d "$file" 2> /dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
        if [ ! -e "$lib/$needed" ] && [ ! -e "$tree/usr/lib64/$needed" ]; then
            echo "fetch-intel-uapi: ${file#"$tree"/} needs $needed, which is not on the volume" >&2
            missing=1
        fi
    done
done < <(find "$tree/usr/bin" "$lib" "$lib/dri" -maxdepth 1 -type f -print0)
[ "$missing" = 0 ] || exit 1

size=$(( $(du -sm "$tree" | cut -f1) + 128 ))
image=$out/intel-uapi.img
rm -f "$out/intel-uapi.stamp" "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
echo "$version" > "$out/intel-uapi.stamp"
echo "intel-uapi volume: $image (${size} MiB), unpacked in $tree"
