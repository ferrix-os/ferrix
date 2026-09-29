#!/usr/bin/env bash
# Build yserver, the X server Steam draws through, and put it on a btrfs
# volume with the Debian libraries it runs on: the disk `cargo xtask
# test-yserver` starts it from (docs/YSERVER.md §3).
#
# yserver's binary links libdrm, gbm, libinput, udev, xkbcommon, freetype and
# fontconfig, and renders through Vulkan, so it is built as an x86-64 glibc
# program against Debian 13's -dev packages, unpacked into a sysroot here, and
# runs on the same packages' libraries from the volume, as Chrome does. Mesa's
# lavapipe is its Vulkan on a machine with no GPU for it, and x11-utils'
# xdpyinfo, xev and xfontsel are the clients the tests run against it, with
# xdotool to unmap, resize and map a window, and to hold a menu open, the way
# a program or a hand would, and xclip to copy and paste through X's
# selections.
#
# The source is the customer's fork of yserver (docs/YSERVER.md §8), pinned
# by commit: 1.6.0 and Ferrix's commits on top. It is built with its
# `wayland` feature, the rootless backend, which takes Ferrix's own Wayland
# client runtime from this repository on GitHub at the commit the fork pins. glslc, which compiles
# yserver's shaders during the build, is Debian's too, unpacked into a
# second tree with its own loader so the host's glibc does not matter.
#
# Every Debian package is pinned by its SHA-256, taken from trixie's,
# trixie-updates' and trixie-security's Packages files on 2026-09-27.
# After unpacking, every library yserver and the libraries beside it need
# is looked for on the volume, so a dependency this list lacks fails here
# rather than on Ferrix.
#
# Writes $FERRIX_YSERVER_VOLUME/yserver.img (default
# ~/.local/share/ferrix/yserver/yserver.img). Needs git, curl, sha256sum,
# dpkg-deb, readelf, strip, mkfs.btrfs and rustup's cargo with the
# toolchain Ferrix pins; no root.
#
# Usage: scripts/fetch/fetch-yserver.sh

set -euo pipefail

debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
security=${DEBIAN_SECURITY_MIRROR:-https://security.debian.org/debian-security}
out=${FERRIX_YSERVER_VOLUME:-$HOME/.local/share/ferrix/yserver}

# The fork, and the commit of it that is built. YSERVER_REPO may name a local
# clone, for a commit not yet pushed.
repo=${YSERVER_REPO:-https://github.com/SetZero/yserver.git}
YSERVER_COMMIT=c5b59356ec1c22b6fcdd24917a5ccdf7cc268080
# The toolchain Ferrix pins in rust-toolchain.toml.
toolchain=${YSERVER_TOOLCHAIN:-1.97.1}

# Pool path and SHA-256 of each package of the sysroot and the volume, from
# the main archive and trixie-updates. The -dev packages are what the build
# links against; the rest is what those and x11-utils and Mesa's Vulkan
# drivers depend on.
debs=(
    "pool/main/f/fontconfig/fontconfig-config_2.15.0-2.3_amd64.deb 0475c00d02660c07a15085051818625331bb502e053242106aaaa1f2ddb41225"
    "pool/main/f/fonts-dejavu/fonts-dejavu-core_2.37-8_all.deb 86635b3d25b3655fc11cb3ecc3af59f0bf19643b02b94f2de48bd10253cdba12"
    "pool/main/f/fonts-dejavu/fonts-dejavu-mono_2.37-8_all.deb 3003e98a5debfdeadc7040a7f715fe9fe6fb67f68deacf6049b54e30f07fc014"
    "pool/main/g/gcc-14/gcc-14-base_14.2.0-19_amd64.deb 5b6825de4263824b78c4c51f6476414f3b4e89c2ab63e81dc8b9b5501e867cf6"
    "pool/main/g/glib2.0/gir1.2-glib-2.0_2.84.4-3~deb13u5_amd64.deb 90f674a980f28f0062cf7bf74a77a8dce728aa57d0915eb12c9cf6ea85ed774e"
    "pool/main/g/glib2.0/gir1.2-glib-2.0-dev_2.84.4-3~deb13u5_amd64.deb 3b49adb163111011070d713324c595d854d7b8b367b310ea92d80a5ab71cd663"
    "pool/main/libg/libgudev/gir1.2-gudev-1.0_238-6_amd64.deb 3deeaae0728e06b5746cab0b8f621986d6355ec09ca1130719c7be1900b1171b"
    "pool/main/g/glib2.0/girepository-tools_2.84.4-3~deb13u5_amd64.deb 2498e412b01d5d77049829d525b7ff559fd4be269a59cc4a50479337a50d8619"
    "pool/main/g/gcc-14/libatomic1_14.2.0-19_amd64.deb 212b399aae2f7299203d261a57e49372e09565a9a5ea971905f94a3960366c05"
    "pool/main/b/brotli/libbrotli-dev_1.1.0-2+b7_amd64.deb 7149124783ccc38764f8b869ea2e4bde319c3d0872b248b7ab472b47d41c5176"
    "pool/main/b/brotli/libbrotli1_1.1.0-2+b7_amd64.deb 0fb79f88db210afbd69282ab9649e525f393ec6950ca34da1a6b359250b8d7db"
    "pool/main/libb/libbsd/libbsd0_0.12.2-2_amd64.deb e5a85986fa6bec3307ab1bc860736b478b331882bc45e17675a7bdf88eecb43a"
    "pool/main/b/bzip2/libbz2-1.0_1.0.8-6_amd64.deb cba4cda04244b5e481bb15524bc3c983a7d1b6f330013b9b381706a2fcb65310"
    "pool/main/b/bzip2/libbz2-dev_1.0.8-6_amd64.deb b395471bf69087a18a6baa04d68a9d50497b20f8a2602d9729c7069f820bbe6b"
    "pool/main/g/glibc/libc-dev-bin_2.41-12+deb13u4_amd64.deb 2c21175d6a8283ed566d154c0c0bc06f60836e53b0b9e1f74c14eac586b68c24"
    "pool/main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb 967aa62605721081c3eb2a17650611a792aa802d76a6511d1840242623d204c9"
    "pool/main/g/glibc/libc6-dev_2.41-12+deb13u4_amd64.deb 1fda734dabcd80b77266a09ab62b0f1e3e16d8091db62899890b2200745632a2"
    "pool/main/libc/libcap2/libcap-dev_2.75-10+deb13u1+b3_amd64.deb 795983cf2cbc2e12e9440383d701229966c35bc40712ccbfd03e5c93d9af619d"
    "pool/main/libc/libcap2/libcap2_2.75-10+deb13u1+b3_amd64.deb 89fc4d34fc7a28ad6f0fcd0c561ab253b9dedf6f77f5a000b47c276c8295bf67"
    "pool/main/libx/libxcrypt/libcrypt-dev_4.4.38-1_amd64.deb 98e2333aea8d64ca68f9b75c16256d3a05492fd8f9e6dba96adbc6f19e4f5a09"
    "pool/main/libx/libxcrypt/libcrypt1_4.4.38-1_amd64.deb 0ebc144d662e3197982d1bf3a7b8b35ca845e54c68811de0328b1f0d7c67585c"
    "pool/main/libd/libdrm/libdrm-amdgpu1_2.4.124-2_amd64.deb c1d97a5e32e2bc68e833b8abeae026b6306198e87ee4396a7ffecd4779caefbf"
    "pool/main/libd/libdrm/libdrm-common_2.4.124-2_all.deb 9a8a6c65c165e9964f106fb4ac710959b5d33e0790227e3ab6b27c4742d1254a"
    "pool/main/libd/libdrm/libdrm-dev_2.4.124-2_amd64.deb 83b84b1207c4dda3d64ebad7725a7ad237c69c870668db3b01eabad1052f0dcb"
    "pool/main/libd/libdrm/libdrm-intel1_2.4.124-2_amd64.deb 188ab1fd74c838b3c8055d62107351e16a4bb7a25d39c30bbb9aac30ddd37238"
    "pool/main/libd/libdrm/libdrm-nouveau2_2.4.124-2_amd64.deb 634dcb46487fb2ca267850261291463845ada90b81d16a926ca40d333ce57180"
    "pool/main/libd/libdrm/libdrm-radeon1_2.4.124-2_amd64.deb 7a82b1c88b45235e314a15b71781b47ed472dafbbf697481ec8427079840fc32"
    "pool/main/libd/libdrm/libdrm2_2.4.124-2_amd64.deb fe2276901c7cd7b8079de63072d37fe1cbeb4eb001a3bc1f1d662ad89aa0890e"
    "pool/main/libe/libedit/libedit2_3.1-20250104-1_amd64.deb b002ea172b9c1e34a67bc497c523c67bb74c3f0a4e98113cb083990a1f1d3bfe"
    "pool/main/e/elfutils/libelf1t64_0.192-4_amd64.deb 94497b7e17b6f574a0605b380d454e20d3f01a9c63b70c2a2263f679d30053e1"
    "pool/main/libe/libevdev/libevdev-dev_1.13.4+dfsg-1_amd64.deb daf65e62433c1154ffab87a8cfdf106efcb20095142134f972923277fe600797"
    "pool/main/libe/libevdev/libevdev2_1.13.4+dfsg-1_amd64.deb 02e85fb9461b85b23f775d5fcf5a31c55d37bec40b63fd98ca5d3a2763714ae7"
    "pool/main/libf/libffi/libffi-dev_3.4.8-2_amd64.deb 76b2b80193a656733e1408ebec5371334be5fd48eb5f50ab99d6baf6081d0f1b"
    "pool/main/libf/libffi/libffi8_3.4.8-2_amd64.deb 0ebdc340de33333639c3c63874cd4b15ac2e83dfa1ef3053b7eefaf4919f4f68"
    "pool/main/f/fontconfig/libfontconfig-dev_2.15.0-2.3_amd64.deb e7c533d87da316da3b127d136c0d1f0238a936c4b0797cb5ab67ebac3cf7151e"
    "pool/main/f/fontconfig/libfontconfig1_2.15.0-2.3_amd64.deb 7ae91ec59857cc8e375eb2bdd371a4a69daea38f7787d06d474a847867d852b1"
    "pool/main/m/mesa/libgbm-dev_25.0.7-2+deb13u1_amd64.deb 8e3f9958fbcd4838953ebc7cb11e661e26652059473278d8f7e130c246f97c2a"
    "pool/main/m/mesa/libgbm1_25.0.7-2+deb13u1_amd64.deb 31fb6d76b9ceaf13848fa617df53f85f62626b4fe7464a93811c720af6d5f2dd"
    "pool/main/g/gcc-14/libgcc-s1_14.2.0-19_amd64.deb 3c71917b490d1a17aed43196a2787a256ecf060526cdb20216a74bedc061b150"
    "pool/main/g/glib2.0/libgio-2.0-dev_2.84.4-3~deb13u5_amd64.deb a9bbd748ddd40a08b336e8d6b1755e698957ac640d081ddc4c330b716c6b89c3"
    "pool/main/g/glib2.0/libgio-2.0-dev-bin_2.84.4-3~deb13u5_amd64.deb d43dddb9b306cb8427bd383e100b8510da8cfa03b5968624f3787bc66a5a95b8"
    "pool/main/g/glib2.0/libgirepository-2.0-0_2.84.4-3~deb13u5_amd64.deb c87a2d4dac81be719fb3dc2e3a1d78376151d8c5c34071e08c10304b8d1fd3c3"
    "pool/main/libg/libglvnd/libgl1_1.7.0-1+b2_amd64.deb 87fa2f6e5abaed4ed385fac879c8dd735af719ee2300222d901793c66e041678"
    "pool/main/m/mesa/libgl1-mesa-dri_25.0.7-2+deb13u1_amd64.deb 722fad9944bdfd29eb6421576331a2697f83bc4b3551d693edb7aefdde6bb198"
    "pool/main/g/glib2.0/libglib2.0-0t64_2.84.4-3~deb13u5_amd64.deb e2baf92c57d1db1753e5781a61450d9a01e7833c7bd2dfe87776a6811c11ecc2"
    "pool/main/g/glib2.0/libglib2.0-bin_2.84.4-3~deb13u5_amd64.deb 733e0bbb4aaa7846a23a02cc3f6af71b41cdfcc833538aae0c35eb2c1160c15a"
    "pool/main/g/glib2.0/libglib2.0-data_2.84.4-3~deb13u5_all.deb 11c671d0a21dd99215bc62fc0219a1fcbb1482f36329ab6b85732e09ddff9e1c"
    "pool/main/g/glib2.0/libglib2.0-dev_2.84.4-3~deb13u5_amd64.deb ad98b1a769da55a8007ea9953306b0deb6092205d4fac8d915e2bd83d4ecc8a5"
    "pool/main/g/glib2.0/libglib2.0-dev-bin_2.84.4-3~deb13u5_amd64.deb d35ef75a4b04ea10081d484e025b0a4fecd0232ffc0e76403bcd249b3b47e643"
    "pool/main/libg/libglvnd/libglvnd0_1.7.0-1+b2_amd64.deb 887f74008166549ce9e100c906aa937e95d6e5ce1c8d86efe8c95fd953359b9c"
    "pool/main/m/mesa/libglx-mesa0_25.0.7-2+deb13u1_amd64.deb 2e6ab57de9931e890621d3ade04213b06e7142a4ae8a72a1756c44d0271aaeb7"
    "pool/main/libg/libglvnd/libglx0_1.7.0-1+b2_amd64.deb 2721fdca0fe3bd963cb39482eabc253af52b88f4a7f6dbb69e475549daf5af3b"
    "pool/main/libg/libgudev/libgudev-1.0-0_238-6_amd64.deb 890a55ea26f52062f377389e6326abeadb72a14741c95ab6a1c5a19c77c3c3d0"
    "pool/main/libg/libgudev/libgudev-1.0-dev_238-6_amd64.deb 108abbee46325fa4f4f38bd8881f624e7f856152e804ef3f0b61cd959de1272d"
    "pool/main/libi/libice/libice6_1.1.1-1_amd64.deb 38185997162793b5dfb768644badc57852879784a2f9d83c6c4e47efa5e36698"
    "pool/main/l/llvm-toolchain-19/libllvm19_19.1.7-3+b1_amd64.deb db0d614d61345ca710fb73e75105f1ec6e38898fa7b3aa99fa7eb7d8b0a11d57"
    "pool/main/x/xz-utils/liblzma5_5.8.1-1+deb13u1_amd64.deb 1cfcc6e0dc36f438a79b6e2189facdb9d150b08f57d190a60e01c98075c7f896"
    "pool/main/libm/libmd/libmd0_1.1.0-2+b1_amd64.deb 7244ec3839b61fac0c1884fe08aaa040f26e8f1f35f1f5d3482eacefd30d1b44"
    "pool/main/m/mtdev/libmtdev-dev_1.1.7-1_amd64.deb cdf2ce0314d5eb2e4f10165063fac0293111a55d1a545d9d28b3c79d017e8cfa"
    "pool/main/m/mtdev/libmtdev1t64_1.1.7-1_amd64.deb 736089af4684b6fe2fc824cb118c82a9722d6a31ad45f845998debe37714cbe5"
    "pool/main/libp/libpciaccess/libpciaccess-dev_0.17-3+b3_amd64.deb dfaa687d4ccb943f5b222261a11a1678992bc499abe472d539aa59c4787a0c33"
    "pool/main/libp/libpciaccess/libpciaccess0_0.17-3+b3_amd64.deb d9a0091071635a84e837051e4813005ac445071731becea28fba4d9806df5252"
    "pool/main/p/pcre2/libpcre2-16-0_10.46-1~deb13u2_amd64.deb 0343948654468fc4b273077c464cb77afeee29fc493c0d4c9d81cd19d5bc4e23"
    "pool/main/p/pcre2/libpcre2-32-0_10.46-1~deb13u2_amd64.deb e481279c1665e03a41bba6eb776baf8d4d78cd20b7906a7fc1591ad92ace94f4"
    "pool/main/p/pcre2/libpcre2-8-0_10.46-1~deb13u2_amd64.deb 1252b96a5bc44bb5db982bef8eb18e54f5047cede2aff641bce4f8e1edb91c3e"
    "pool/main/p/pcre2/libpcre2-dev_10.46-1~deb13u2_amd64.deb f031c53b8f77c2825d9d6c71c0d8bd6106d6599af1655074b62074a10f7f7c6c"
    "pool/main/p/pcre2/libpcre2-posix3_10.46-1~deb13u2_amd64.deb f79397e7c5d6e7e3895d89ab1d315fc6313be4d120e7ac008d598931219f1387"
    "pool/main/p/pkgconf/libpkgconf3_1.8.1-4_amd64.deb 85087cd04e57fd4ab7d6e816d348c335047823ab60c78878336b74f07b352ca1"
    "pool/main/libs/libselinux/libselinux1_3.8.1-1_amd64.deb 68bb8d32bd8d6d7d2f5952a169db03d1484b46ae1e52abccdec42a19dccea5d5"
    "pool/main/libs/libselinux/libselinux1-dev_3.8.1-1_amd64.deb 0107595c65dd5cfe7ce3643d400e7b2be2c2528bcd110ad5af8605711a858921"
    "pool/main/l/lm-sensors/libsensors-config_3.6.2-2_all.deb 3056da80c7d963af795dab480ab6f6f4b154ad4ac39f522dc52d17c834fea253"
    "pool/main/l/lm-sensors/libsensors5_3.6.2-2_amd64.deb f0a994a6d7cfa695dea5343d0d1ba7eed796c0ad920c7282998b95f60049c4f6"
    "pool/main/libs/libsepol/libsepol-dev_3.8.1-1_amd64.deb c085881d0546f6d539909f5710d7a3243aeb24338f6dc455b0ef6199f45bd753"
    "pool/main/libs/libsepol/libsepol2_3.8.1-1_amd64.deb 3595d2d3a6d24695e7953f4f00cdfe6974c9242d9a8dfee8998e77fbf7b2ba09"
    "pool/main/libs/libsm/libsm6_1.2.6-1_amd64.deb 03038b3002e6f44322554e335d60d8ff87acfcf2e21c904b6866610a9913ee00"
    "pool/main/g/gcc-14/libstdc++6_14.2.0-19_amd64.deb ab1fa05837aa7a92aae748fd07a18a35f7d18bb4a71c4724fe2bbf0e32089de0"
    "pool/main/s/sysprof/libsysprof-capture-4-dev_48.0-2_amd64.deb 3cb76e124187700f5977a0acb914b3f82b2bd28105fce6f1cf2a34497b234af2"
    "pool/main/n/ncurses/libtinfo6_6.5+20250216-2_amd64.deb 8b9f6a7983e9418564e48a627518de4c03917b56efe68d7f3e93bd8fffa1cc10"
    "pool/main/s/systemd/libudev-dev_257.13-1~deb13u1_amd64.deb 73c3ed98a435ec420b327ef267d89ece81bf23c2c19391265bb4f261cbf5d558"
    "pool/main/s/systemd/libudev1_257.13-1~deb13u1_amd64.deb 5d41c284f5a93b05bc7d648b61a02dd2bb9ff05b2261ad1a8b7d96044a0cfa88"
    "pool/main/v/vulkan-loader/libvulkan1_1.4.309.0-1_amd64.deb f47da79cd140264fe21cceb08bc87a71bc7fec05819e4a421f3f518d21101a37"
    "pool/main/libw/libwacom/libwacom-common_2.14.0-1_all.deb 214956bd9b26600c4d5ac0d60c1685be6ef650644e36b712620c56fccd4d4f58"
    "pool/main/libw/libwacom/libwacom-dev_2.14.0-1_amd64.deb 19d2f8d2aa6b7eae42397cdd64f19ccd5de6c08c7681bd559624c3067c5f3358"
    "pool/main/libw/libwacom/libwacom9_2.14.0-1_amd64.deb 884cb03fe12ffb871f9a199c13d1a455c5abb30b4619c58babb3940755b7b3c8"
    "pool/main/w/wayland/libwayland-client0_1.23.1-3_amd64.deb d1607e1db1a7c5378a2e5c6ae7b20f64dc6acf134f953d84a7f5204b74339f33"
    "pool/main/w/wayland/libwayland-server0_1.23.1-3_amd64.deb 2967212bd582e0dffca443fdc44f4c660e7368d41f7ee3a7f6314e0c3abfe9ea"
    "pool/main/libx/libx11/libx11-6_1.8.12-1_amd64.deb b5a3fd3bf8c8fd0364bfb9bea00dcba7fc301229bd02dded084632d31f5b0fb3"
    "pool/main/libx/libx11/libx11-data_1.8.12-1_all.deb c54f87069888f80ba4da586da6147d74c7598ccdd8b90906dbc4271fa414c738"
    "pool/main/libx/libx11/libx11-xcb1_1.8.12-1_amd64.deb e05f94d21a932fba5b09b9b13d99df776b155d3bc792c0e294451df9ffe1ba25"
    "pool/main/libx/libxau/libxau6_1.0.11-1_amd64.deb 689a9f0e0ba3e2c65431f864871e303ee904de69dd28abfc462663fae030227f"
    "pool/main/libx/libxaw/libxaw7_1.0.16-1_amd64.deb e79f3ab51ecbbfd3414a0568a8947ac59a29c30d631e6c7b6e07ea46e688c047"
    "pool/main/libx/libxcb/libxcb-dri3-0_1.17.0-2+b1_amd64.deb f446d42fb5fcebbb3e347368ba83616769fdb271d85b2f49048e337f3163d267"
    "pool/main/libx/libxcb/libxcb-glx0_1.17.0-2+b1_amd64.deb cc59a3fa1c4ce376c3a32798179950810bd7b9be9a35bc76de1f5b5e7cc3925e"
    "pool/main/libx/libxcb/libxcb-present0_1.17.0-2+b1_amd64.deb db95ea4630c55bd7f3281cb60ccf75b1627ef2f4399e1939f8f6161e584a92fb"
    "pool/main/libx/libxcb/libxcb-randr0_1.17.0-2+b1_amd64.deb f5d9fe5fdf797918f81f0abf6cfd4270bd54659540cddb4da709ab7154523214"
    "pool/main/libx/libxcb/libxcb-shape0_1.17.0-2+b1_amd64.deb eb94a643d66714929053e78ff8f38a90ce8af4ed854e3cbca0b14f8bbccc7bfc"
    "pool/main/libx/libxcb/libxcb-shm0_1.17.0-2+b1_amd64.deb d1777a813f484e89f3dcaadd1d5f2b1a13e9313b6bf70eded16d8ce743208fef"
    "pool/main/libx/libxcb/libxcb-sync1_1.17.0-2+b1_amd64.deb 0ce4770ed1505be9ddc6473045e4662d062aff2f3077ee684265f01cfb543559"
    "pool/main/libx/libxcb/libxcb-xfixes0_1.17.0-2+b1_amd64.deb f9c1aafc18bf9e4662e34bbaf3bb00f1f4e8c2fc1dd786fdfb6cb9cb6c64fae7"
    "pool/main/libx/libxcb/libxcb1_1.17.0-2+b1_amd64.deb 5c222a72d11b866447da31693254f738430726e3e065a384e82687b2fd2f978b"
    "pool/main/libx/libxcomposite/libxcomposite1_0.4.6-1_amd64.deb 20e3c1d9b2135f0c8c4246a9fd26a51a57d5850c3b36ecc173c45d6be7328af3"
    "pool/main/libx/libxdmcp/libxdmcp6_1.1.5-1_amd64.deb 0740dc760916b2008b45417a42a8fd7dd5de370fb57d31373f15034cda8acf0b"
    "pool/main/libx/libxext/libxext6_1.3.4-1+b3_amd64.deb fc618ec40465e5ce48622606299cb47833efc3fb235ba15543b81f850722f443"
    "pool/main/x/xft/libxft2_2.3.6-1+b4_amd64.deb 0226da7b9186ab7a9f78efcbbefa7ce3b3b398745e1909e124a3d58471fa0edc"
    "pool/main/libx/libxi/libxi6_1.8.2-1_amd64.deb 093d0903f35bb7a9f6815180ee040e6951fecf9b66c128cd72f064710210606e"
    "pool/main/libx/libxinerama/libxinerama1_1.1.4-3+b4_amd64.deb 49d4e1628960407b8be2880a42428e7629801715a8a5697d42c0b0ca0a479911"
    "pool/main/libx/libxkbcommon/libxkbcommon-dev_1.7.0-2_amd64.deb b62327152ae59bec442c0d7ae893520a3a3a2381100c83332e4a3497157ce5ec"
    "pool/main/libx/libxkbcommon/libxkbcommon0_1.7.0-2_amd64.deb f75ee544f55acc6a271debfab3ea4ae0458afc89d81cfe1a71137e07d4895b86"
    "pool/main/libx/libxkbfile/libxkbfile1_1.1.0-1+b4_amd64.deb 989b61e0eb7f1f99d0e969a5957e9a857a95941d5c0e86d5e2b502008b4ac28e"
    # The build the rustc volume has (2026-09-28), so --everything's merged
    # volume holds one libxml2; security's deb13u1 was superseded.
    "pool/main/libx/libxml2/libxml2_2.12.7+dfsg+really2.9.14-2.1+deb13u3_amd64.deb e0c6b63ce4602a036a526f60fe5e6c1586710688058d98fc1001b9b3147b7efd"
    "pool/main/libx/libxmu/libxmu6_1.1.3-3+b4_amd64.deb cfdf1dee8f9abd87f503222fc8b04a30ce820fb1255247b7c3a28ff9c244431a"
    "pool/main/libx/libxmu/libxmuu1_1.1.3-3+b4_amd64.deb 6b5f537905421599e962a8cec68c84f06d506e143fc70b0b570380b3c4901678"
    "pool/main/libx/libxpm/libxpm4_3.5.17-1+deb13u1_amd64.deb d2f8933b552e7a282bbc32bc7545628195c4a3692a20ad0e232cc5fee9130790"
    "pool/main/libx/libxrandr/libxrandr2_1.5.4-1+b3_amd64.deb 11e3490de93a8bbee3daba719cb8e1325a26fb3c125525c34bdcb7deb05eb9b2"
    "pool/main/libx/libxrender/libxrender1_0.9.12-1_amd64.deb 9d042dfd5e613be1e02e6ddd0c5c4adef19c5eb08f6db838c2eba672c496dca4"
    "pool/main/libx/libxshmfence/libxshmfence-dev_1.3.3-1_amd64.deb 52a0a738552cd340c6cf237341e708ceecb1a2547307131d214c20de816be423"
    "pool/main/libx/libxshmfence/libxshmfence1_1.3.3-1_amd64.deb 7b339e9e5b2349723d35af4df89bcc7aa456bbdf8ba1754358f9b44c3fe1f964"
    "pool/main/libx/libxt/libxt6t64_1.2.1-1.2+b2_amd64.deb a1f4557c03113681ee24863495adc244cecec95fe3ab7256ce76e87320157300"
    "pool/main/libx/libxtst/libxtst6_1.2.5-1_amd64.deb 4f254f80b8984203474745d6428e8c2de2148f19b37aa01a601dbe9ac03196eb"
    "pool/main/libx/libxv/libxv1_1.0.11-1.1+b3_amd64.deb e19f6bdcd20c1c28fefea29f0a95c33232dd5f0d26e0079675ac8ba4af4eda07"
    "pool/main/libx/libxxf86dga/libxxf86dga1_1.1.5-1+b3_amd64.deb 5585791fed815875b6d9cfbcd48a4df454472696c5b1b6d4240b6c53d80d75b2"
    "pool/main/libx/libxxf86vm/libxxf86vm1_1.1.4-1+b4_amd64.deb f9f8487e536293e7ecf91fe7feb14e97e02a3aa652e207c661442dce775d3b14"
    "pool/main/z/z3/libz3-4_4.13.3-1_amd64.deb 71383373523ef62d47eccf660cf6535c3febcbb3f88e54cb7a43124014b57359"
    "pool/main/libz/libzstd/libzstd1_1.5.7+dfsg-1_amd64.deb 2f6a2aeacfc925eba8b00ac9139bc4bfccf8cacb09eb93de067074b26948eef9"
    "pool/main/l/lsb/lsb-base_11.6_all.deb f8bedd167280e76636df3a1bc023cd2906d458916c1af4c1d7912c5b971fc642"
    "pool/main/m/mesa/mesa-libgallium_25.0.7-2+deb13u1_amd64.deb 3e610f29321cdcc61337c86e6b4031ff60f0c9915fe19e6a862ac06480620ff4"
    "pool/main/m/mesa/mesa-vulkan-drivers_25.0.7-2+deb13u1_amd64.deb caf2f7d296b7522efe74e40a74bce762a3ce84c33b53b5e54e1248ac2e0a13a5"
    "pool/main/a/architecture-properties/native-architecture_0.2.6_all.deb cb065efb2dd0ad8de45ace298bdb959e4d66478ca8599312bde4fa543d8b7532"
    "pool/main/p/pkgconf/pkgconf_1.8.1-4_amd64.deb f1fcad470ca3ac80b4a0f4af6f03cd215089473996b07e508b3ee342bbb11af7"
    "pool/main/p/pkgconf/pkgconf-bin_1.8.1-4_amd64.deb 54efef2aef4db5fcb5e0f5b7746465000c6a779b06f3686d0ec3d2a901b20aeb"
    "pool/main/p/python-packaging/python3-packaging_25.0-1_all.deb d70c8469f6e9c6105e1ebcd3ebdc963f11dadbeddd89d2d1a2a3cb1c97d2ecbc"
    "pool/main/r/rpcsvc-proto/rpcsvc-proto_1.4.3-1_amd64.deb 32ac0692694f8a34cc90c895f4fc739680fb2ef0e2d4870a68833682bf1c81a3"
    "pool/main/s/sysvinit/sysvinit-utils_3.14-4_amd64.deb c7e957360dff89675b491e501b00047cf750695178cabab167c92fc260d358ec"
    "pool/main/x/xorg/x11-common_7.7+24+deb13u1_all.deb 57a981938506b26dc552f19cafeaa8c04e9b59dc7508509151f8507a9d4e5f24"
    "pool/main/x/x11-utils/x11-utils_7.7+7_amd64.deb 239d8a87b464cbe370d63029820218a52c5bfa1f0dfeb74f7cfe7d0df017046b"
    "pool/main/x/xkeyboard-config/xkb-data_2.42-1_all.deb 196ff18533382f64e057ea49df2bb486bd4275a4cc0917361edb560b8756dada"
    "pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_amd64.deb 015be740d6236ad114582dea500c1d907f29e16d6db00566ca32fb68d71ac90d"
    "pool/main/z/zlib/zlib1g-dev_1.3.dfsg+really1.3.1-1+b1_amd64.deb 76ed5c858e1aef38b7be93acce5910e457e77bd6271471b9d05fb42e78826224"
    "pool/main/x/xdotool/libxdo3_3.20160805.1-5.1_amd64.deb d634e25d2b50af140ee9f44bab507fcfb665f3556fd483faefdb56e85598cc12"
    "pool/main/x/xdotool/xdotool_3.20160805.1-5.1_amd64.deb 98ac8681533b01020c7288fb2e9de403d586747dab48cfd58e04cb00d0c44fb1"
    "pool/main/x/xclip/xclip_0.13-4_amd64.deb 14203d9a0138996c647d603fcadd79e6765a0a124f606e386215325ac13ece76"
)

# The same, from the security archive.
security_debs=(
    "pool/updates/main/u/util-linux/libblkid-dev_2.41.5-0+deb13u1_amd64.deb 775bc91f7a265d21cdfc6b710813017b2366e5c5bb665cd2e6979af5dd8a03a8"
    "pool/updates/main/u/util-linux/libblkid1_2.41.5-0+deb13u1_amd64.deb 81535f3c2c0efc732965907c8749103a0a26377c761622c9ce39b4c92dcde52f"
    "pool/updates/main/e/expat/libexpat1_2.8.3-1~deb13u1_amd64.deb 38abe0e710a07688e9c149d74536e67cfee0364bdb64dd6d644c32a1cfad389f"
    "pool/updates/main/e/expat/libexpat1-dev_2.8.3-1~deb13u1_amd64.deb 13f69f5290374c8403d2114b91cc8d0e6bb7b74e9e2512b7e04557546c922749"
    "pool/updates/main/f/freetype/libfreetype-dev_2.13.3+dfsg-1+deb13u1_amd64.deb f58107859d9fa44206e64cf35bdbd4febdec66a33e456ce2e5e3712f01b6c895"
    "pool/updates/main/f/freetype/libfreetype6_2.13.3+dfsg-1+deb13u1_amd64.deb e4947f3291528f03d574f2b01d5c5fc45c58c47480f32888a5cdf0f231ab584e"
    "pool/updates/main/libi/libinput/libinput-bin_1.28.1-1+deb13u1_amd64.deb 0ef9fc75d9fb4d8aa478baa3e2165c8f3b5e7310c13d9dddbb18029e6cad9143"
    "pool/updates/main/libi/libinput/libinput-dev_1.28.1-1+deb13u1_amd64.deb e87786154bb9d8f3450437747b400ebe9ff51125034fa481bd11c966889edb22"
    "pool/updates/main/libi/libinput/libinput10_1.28.1-1+deb13u1_amd64.deb fa967b1a25d8e5c0314459f6abd9e0e7518de7d8e02a8d4e69f51642a88569d4"
    "pool/updates/main/u/util-linux/libmount-dev_2.41.5-0+deb13u1_amd64.deb 2bef03b918e927648c6552143e17e82aa7ed2a4ce180fabd07a7d1cac7ff8686"
    "pool/updates/main/u/util-linux/libmount1_2.41.5-0+deb13u1_amd64.deb 6d00f45f2e80e078e906e3eedecd3ba6913e39fef49bff361ee583f17f00ec05"
    "pool/updates/main/libp/libpng1.6/libpng-dev_1.6.48-1+deb13u5_amd64.deb 9027d5ace59ce266124c87054302805f49f9cb1ec4a6c8264a64596c7916fbff"
    "pool/updates/main/libp/libpng1.6/libpng16-16t64_1.6.48-1+deb13u5_amd64.deb 2465b4e9fa85cff54dc10a6da3e64074d8d9292c86e4a8f989b809d6b158e97e"
    "pool/updates/main/u/util-linux/libuuid1_2.41.5-0+deb13u1_amd64.deb c1bf4c4c3ff48c57fabf93307dfb56996b60cfa33927afc4158b5db36fb2721e"
    "pool/updates/main/l/linux/linux-libc-dev_6.12.101-1_all.deb e776ef48b89af0409c6bcd2e8a7e14a3d75d0842b9fd3dc767b7822a0b0950e2"
    "pool/updates/main/u/util-linux/uuid-dev_2.41.5-0+deb13u1_amd64.deb cb7d7db20e5d91193c2c6629a3f4aa917e3bc558c72b7f2db7203bcfa9a3ba1b"
)

# glslc and what it runs on, in a tree of its own.
tool_debs=(
    "pool/main/g/gcc-14/gcc-14-base_14.2.0-19_amd64.deb 5b6825de4263824b78c4c51f6476414f3b4e89c2ab63e81dc8b9b5501e867cf6"
    "pool/main/s/shaderc/glslc_2025.2-1_amd64.deb e05374965709159303041b104d4a26022d6ac5eb8296450364d66ae864a20367"
    "pool/main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb 967aa62605721081c3eb2a17650611a792aa802d76a6511d1840242623d204c9"
    "pool/main/g/gcc-14/libgcc-s1_14.2.0-19_amd64.deb 3c71917b490d1a17aed43196a2787a256ecf060526cdb20216a74bedc061b150"
    "pool/main/s/shaderc/libshaderc1_2025.2-1_amd64.deb 2a67c8f784c65e1aa0d251819c020917405b490fbb543c6a73a52a631e476463"
    "pool/main/g/gcc-14/libstdc++6_14.2.0-19_amd64.deb ab1fa05837aa7a92aae748fd07a18a35f7d18bb4a71c4724fe2bbf0e32089de0"
)

for tool in git curl sha256sum dpkg-deb readelf strip mkfs.btrfs cargo; do
    command -v "$tool" > /dev/null || { echo "fetch-yserver: $tool is not installed" >&2; exit 1; }
done

# Download $2/$1 into the pool unless it is there, and check it against $3.
fetch() {
    local path=$1 base=$2 sum=$3
    local file="$out/pool/${path##*/}"
    if [ ! -f "$file" ]; then
        curl -fsSL -o "$file.part" "$base/$path"
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { echo "fetch-yserver: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

mkdir -p "$out/pool"
sysroot="$out/sysroot"
tools="$out/tools"
rm -rf "$sysroot" "$tools"
mkdir -p "$sysroot" "$tools"

# Debian 13's merged /usr, which an installed system has and unpacking does
# not make: glibc's linker scripts name /lib/x86_64-linux-gnu/libm.so.6,
# found inside the sysroot only through these links.
for merged in bin lib lib64 sbin; do
    mkdir -p "$sysroot/usr/$merged"
    ln -s "usr/$merged" "$sysroot/$merged"
done

for entry in "${debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$debian" "$sum")" "$sysroot"
done
for entry in "${security_debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$security" "$sum")" "$sysroot"
done
for entry in "${tool_debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$debian" "$sum")" "$tools"
done

# glslc through its own loader and libraries.
cat > "$out/glslc" << GLSLC
#!/bin/sh
exec "$tools/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2" \\
    --library-path "$tools/usr/lib/x86_64-linux-gnu" "$tools/usr/bin/glslc" "\$@"
GLSLC
chmod +x "$out/glslc"

# The source, at the pinned commit and nothing else.
source="$out/src"
if [ "$(git -C "$source" rev-parse HEAD 2> /dev/null)" != "$YSERVER_COMMIT" ]; then
    rm -rf "$source"
    git init -q "$source"
    git -C "$source" fetch -q --depth 1 "$repo" "$YSERVER_COMMIT"
    git -C "$source" checkout -q --detach FETCH_HEAD
fi

# The build, against the sysroot: pkg-config finds the -dev packages' files
# there, and the C compiler and the linker look nowhere else.
(
    cd "$source"
    export PKG_CONFIG_SYSROOT_DIR="$sysroot"
    export PKG_CONFIG_LIBDIR="$sysroot/usr/lib/x86_64-linux-gnu/pkgconfig:$sysroot/usr/share/pkgconfig"
    export PKG_CONFIG_ALLOW_CROSS=1
    export CFLAGS_x86_64_unknown_linux_gnu="--sysroot=$sysroot"
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=--sysroot=$sysroot"
    export CARGO_TARGET_DIR="$out/target"
    export GLSLC="$out/glslc"
    cargo "+$toolchain" build --locked --release --target x86_64-unknown-linux-gnu \
        --features wayland --bin yserver
)
binary="$out/target/x86_64-unknown-linux-gnu/release/yserver"

# The volume: the sysroot less what only a build reads, and the Vulkan
# drivers for hardware a guest does not have -- lavapipe stays, and venus
# (virtio), for the GPU path to come.
tree="$out/tree"
rm -rf "$tree"
cp -a "$sysroot" "$tree"
rm -rf "$tree/usr/include" "$tree/usr/share/doc" "$tree/usr/share/man" \
    "$tree/usr/share/locale" "$tree/usr/share/lintian" "$tree/usr/share/aclocal" \
    "$tree/usr/share/gir-1.0" "$tree/usr/share/gtk-doc" \
    "$tree/usr/lib/x86_64-linux-gnu/pkgconfig" "$tree/usr/share/pkgconfig"
find "$tree" -name '*.a' -delete
for driver in radeon intel intel_hasvk nouveau gfxstream; do
    rm -f "$tree/usr/lib/x86_64-linux-gnu/libvulkan_$driver.so"
done
rm -f "$tree"/usr/share/vulkan/icd.d/{radeon,intel,intel_hasvk,nouveau,gfxstream_vk}_icd.json
mkdir -p "$tree/yserver"
strip -o "$tree/yserver/yserver" "$binary"

test -e "$tree/usr/lib64/ld-linux-x86-64.so.2" \
    || { echo "fetch-yserver: no linker at usr/lib64" >&2; exit 1; }
test -e "$tree/usr/share/vulkan/icd.d/lvp_icd.json" \
    || { echo "fetch-yserver: no lavapipe on the volume" >&2; exit 1; }

# Every library an ELF file on the volume needs must be on it.
missing=0
while IFS= read -r -d '' file; do
    head -c 4 "$file" | grep -q $'\x7fELF' || continue
    for needed in $(readelf -d "$file" 2> /dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
        if [ ! -e "$tree/usr/lib/x86_64-linux-gnu/$needed" ] \
            && [ ! -e "$tree/usr/lib64/$needed" ]; then
            echo "fetch-yserver: ${file#"$tree"/} needs $needed, which is not on the volume" >&2
            missing=1
        fi
    done
done < <(find "$tree/yserver" "$tree/usr/bin" "$tree/usr/lib/x86_64-linux-gnu" -maxdepth 1 -type f -print0)
[ "$missing" = 0 ] || exit 1

# Room for the server's logs and a client's files.
size=$(( $(du -sm "$tree" | cut -f1) + 256 ))
image="$out/yserver.img"
rm -f "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
echo "yserver volume: $image (${size} MiB), unpacked in $tree"
