#!/usr/bin/env bash
# Fetch Debian's Chromium for armhf and every library it loads, and put them
# on a btrfs volume: the disk Chromium runs from on an ARMv7-A Ferrix -- the
# STM32MP157D-DK1 above all (docs/CHROME.md §10).
#
# The armhf twin of tools/common/fetch/fetch-chromium-arm64.sh, made the same
# way. Chrome for Testing publishes linux64 alone; Debian 13 builds Chromium
# for armhf, with its armhf (hard-float EABI) glibc and loader. On
# 2026-10-07 the armhf build is 150.0.7871.181 in both the main and the
# security archive, while amd64 and arm64 have 154: Debian's armhf builds
# have fallen a version behind, so this volume's Chromium is not the x86-64
# and arm64 volumes' version.
#
# The package list is Chromium's dependency closure as
# `tools/common/gen/debian-closure.py --arch armhf chromium --skip ...`
# resolved it from trixie, trixie-updates and trixie-security on 2026-10-07,
# with the arm64 volume's --skip list: debconf, systemd and D-Bus's daemons,
# GTK (which Chromium only `dlopen`s, for its file dialog and theme) and
# Mesa's GL drivers and LLVM (Chromium draws in software here, with
# `--disable-gpu`). 124 packages, 335 MiB installed as Debian counts it.
# Every download is pinned by the SHA-256 its index gave. After unpacking,
# every library each ELF file on the volume needs is looked for on it, so a
# library the closure left out fails here rather than on Ferrix.
#
# The volume is a Debian-shaped tree at its root -- `usr/lib`,
# `usr/lib/arm-linux-gnueabihf`, Chromium in `usr/lib/chromium` -- which
# Ferrix mounts at `/data`; the image links the directories glibc names into
# it.
#
# Room: the arm64 volume leaves 512 MiB free for Chromium's profile. This one
# leaves $FERRIX_CHROMIUM_ROOM MiB (default 64), because on the DK1 a volume
# is held in RAM until the board reads its SD card at run time, and the
# profile goes to tmpfs (`--user-data-dir` in /dev/shm) either way.
#
# Writes $FERRIX_CHROMIUM_VOLUME/chromium.img (default
# ~/.local/share/ferrix/chromium-armhf/chromium.img). Needs curl, sha256sum,
# dpkg-deb, readelf and mkfs.btrfs; no root.
#
# Usage: tools/common/fetch/fetch-chromium-armhf.sh

set -euo pipefail

debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
security=${DEBIAN_SECURITY_MIRROR:-https://security.debian.org/debian-security}
out=${FERRIX_CHROMIUM_VOLUME:-$HOME/.local/share/ferrix/chromium-armhf}

# `<archive> <pool path> <sha256>`: main and updates come from $debian,
# security from $security.
debs=(
    "main pool/main/a/at-spi2-core/at-spi2-common_2.56.2-1+deb13u2_all.deb a2a7b53c9925ec33a471d56dd60f5effe35cece109ec4296cc8d98a1250213b4"
    "main pool/main/c/chromium/chromium_150.0.7871.181-1~deb13u1_armhf.deb 20917f59cecdb7227839b9fe40e8d5127328f48e6a11bc44ace9e306cc94316f"
    "main pool/main/c/chromium/chromium-common_150.0.7871.181-1~deb13u1_armhf.deb 33f49540a61e65ddca79e468da9752efcd2ed0ecf27cc5c7baf767e2016ad5af"
    "main pool/main/f/fontconfig/fontconfig_2.15.0-2.3_armhf.deb 600633bc3b1476177e6383b5355f6c9d99929fee9b18072af840c72912ac6c52"
    "main pool/main/f/fontconfig/fontconfig-config_2.15.0-2.3_armhf.deb 7bddbe10c203be96b3d4c901ee91c1a2460ce2a0b09be2f9ee42b6b14559bd72"
    "main pool/main/f/fonts-dejavu/fonts-dejavu-core_2.37-8_all.deb 86635b3d25b3655fc11cb3ecc3af59f0bf19643b02b94f2de48bd10253cdba12"
    "main pool/main/f/fonts-dejavu/fonts-dejavu-mono_2.37-8_all.deb 3003e98a5debfdeadc7040a7f715fe9fe6fb67f68deacf6049b54e30f07fc014"
    "main pool/main/g/gcc-14/gcc-14-base_14.2.0-19_armhf.deb 0f702fdd5e5471efda9fece892e09ce73e3447968083e1f8c341f8b66b1fb340"
    "main pool/main/a/alsa-lib/libasound2-data_1.2.14-1+deb13u1_all.deb 04688afdff3769c0f685541daed7b2f6f0cb946799ddf1d5847ddf11fe245559"
    "main pool/main/a/alsa-lib/libasound2t64_1.2.14-1+deb13u1_armhf.deb 7216334fad7b64dc619a3aba8910c3ae36ffe0737c9bf94a558caffcaaa7ef80"
    "main pool/main/liba/libasyncns/libasyncns0_0.8-6+b5_armhf.deb ed522a2a22933f9ea9b130036c16846f3b19e9daa8c8c18dae1d054a76f94651"
    "main pool/main/a/at-spi2-core/libatk-bridge2.0-0t64_2.56.2-1+deb13u2_armhf.deb 9714c27c87aec4a7282e1114a77809620e49deb94ccc893cd6205c2d166d7efa"
    "main pool/main/a/at-spi2-core/libatk1.0-0t64_2.56.2-1+deb13u2_armhf.deb ad222a81611c855e941a0735e4db13d0c277c01688574a1f4e8942e110d1c02c"
    "main pool/main/g/gcc-14/libatomic1_14.2.0-19_armhf.deb e4a4dc17913ded43f2a8571f9c8c214d05534632fc5a41705d6b39acd8d5f4a4"
    "main pool/main/a/at-spi2-core/libatspi2.0-0t64_2.56.2-1+deb13u2_armhf.deb 82f6d527920a339694789e2b2888b21aa5504fe3fc10624d0ac4f3625ba8b820"
    "main pool/main/a/avahi/libavahi-client3_0.8-16_armhf.deb 8e89da0ebf77161d1cd79abcf25bbc379f68a4f62dd9ab1b4a2450b0b96b3590"
    "main pool/main/a/avahi/libavahi-common-data_0.8-16_armhf.deb 149826aa7066d6aadcb2040f17be4fdd33d57b8d125864facaa299835660c06e"
    "main pool/main/a/avahi/libavahi-common3_0.8-16_armhf.deb e1ea3629111367398a78639ae430c958ec4ea59533206cb0090cb1edda50d115"
    "main pool/main/u/util-linux/libblkid1_2.41.5-0+deb13u1_armhf.deb 6372515565c97d522eed8e732e70b58501e27ca626b29fcdbcccc9ac0fbb6142"
    "main pool/main/b/brotli/libbrotli1_1.1.0-2+b7_armhf.deb edf61079f84af123801453706e14de8aa0e3a2f160d5f9975ac6053bd3f7638c"
    "main pool/main/b/bzip2/libbz2-1.0_1.0.8-6_armhf.deb 26f1ff3f79069f1f9bbbf2f6fb6d61694560500d9eb803cf31669a678d3b5212"
    "main pool/main/g/glibc/libc6_2.41-12+deb13u4_armhf.deb 4fc6fed8d77d01c0dacb5fda8b880a8c8a669575936de5761b693077a6f6c2ec"
    "main pool/main/c/cairo/libcairo2_1.18.4-1+b1_armhf.deb 6f9a37878b11b4fe703992b340bab40f4898c61b393c6bb889089d241c835ccd"
    "main pool/main/libc/libcap2/libcap2_2.75-10+deb13u1+b3_armhf.deb 657d0b1d0f43bac7143f6166bd1efd9e13619c9d0c68a2d98bf5bbc15a9a09b0"
    "main pool/main/e/e2fsprogs/libcom-err2_1.47.2-3+b12_armhf.deb 684cf84d3c6d5da50952119aaeaa1be1a853d8de794589d8313a0680dd683f24"
    "main pool/main/c/cups/libcups2t64_2.4.10-3+deb13u2_armhf.deb 9e830bf9d1a882e6da9b4e085eda830ae4b743466852ffe1c240bdd2573a8bfa"
    "main pool/main/libd/libdatrie/libdatrie1_0.2.13-3+b1_armhf.deb 1104e32cd90bc8722b02085c5ab6b3c29b27895ac8a064c89002eb1d7eeafef0"
    "main pool/main/d/dav1d/libdav1d7_1.5.1-1_armhf.deb e4511ec6fc1e233bff0a119f1fc7decb1cd1a2089f04669c81db91dba9766254"
    "main pool/main/d/dbus/libdbus-1-3_1.16.2-2_armhf.deb e76db629fa838da33c96f42285df2b60b78fa5f916e841d59ed4efb014145a3b"
    "main pool/main/d/double-conversion/libdouble-conversion3_3.3.1-1_armhf.deb 041f3a4a8f4b72073efc0e7d4e27143e4cde8a3ac329ee5c832fe0d0ea30f941"
    "main pool/main/libd/libdrm/libdrm-common_2.4.124-2_all.deb 9a8a6c65c165e9964f106fb4ac710959b5d33e0790227e3ab6b27c4742d1254a"
    "main pool/main/libd/libdrm/libdrm2_2.4.124-2_armhf.deb 27a8de17eb84a453b43689754c0bb0d71598011746f802785aaa1573c95fa0ee"
    "main pool/main/e/expat/libexpat1_2.8.3-1~deb13u1_armhf.deb b24918918933a0efa8a99db182e190e50c04fdc74fecb52430d1ac8d03332606"
    "main pool/main/libf/libffi/libffi8_3.4.8-2_armhf.deb 502d2edefd34dfb2a2010b5c131dee6ee0b4709facb55b9a336493747695645a"
    "main pool/main/f/flac/libflac14_1.5.0+ds-2_armhf.deb 52edf9f74290910d79ad5a879e297a1f5e548c543e7e5c24c029247257a40038"
    "main pool/main/f/fontconfig/libfontconfig1_2.15.0-2.3_armhf.deb 4412d91a1804bc237850ad56824e3233e867ebb9626b52a86d9a71105c21aee9"
    "main pool/main/f/freetype/libfreetype6_2.13.3+dfsg-1+deb13u1_armhf.deb 013ed9ed6760400b100dfc54008d779b02075b42bddc0cf0a11172ec8c28dbd9"
    "main pool/main/f/fribidi/libfribidi0_1.0.16-1_armhf.deb 15bf4cd5825cc86885d9321db18fb551a525a388f5a10d7e88eb132f689e4689"
    "main pool/main/m/mesa/libgbm1_25.0.7-2+deb13u1_armhf.deb 97e69a6b5fc1a7d3e1d0c1fc4b877b124572a5c314f5c6dd219e8804eeee625f"
    "main pool/main/g/gcc-14/libgcc-s1_14.2.0-19_armhf.deb 25910c3a0bce3985e388de445244578b7922dc670c9bba1c8673779b89447873"
    "main pool/main/g/glib2.0/libglib2.0-0t64_2.84.4-3~deb13u5_armhf.deb 1f525dd2c41279a62b273affca6018a6f89b2d1e9831884eeec531dc17b8af4f"
    "main pool/main/g/gmp/libgmp10_6.3.0+dfsg-3_armhf.deb b74d0fa0aa9d1e2f7addae5d3c235fc881e1507d98559ab158362bef56877a56"
    "main pool/main/g/gnutls28/libgnutls30t64_3.8.9-3+deb13u4_armhf.deb 204b80443716380a70e113e2be98519d7c3a8cb00125cca631337032842f4627"
    "main pool/main/g/graphite2/libgraphite2-3_1.3.14-2+deb13u1_armhf.deb 4be03dfee3dc1552977b9a053437641f029fe36ba069dd930ea2c121722df46f"
    "main pool/main/k/krb5/libgssapi-krb5-2_1.21.3-5+deb13u1_armhf.deb df4ca163a4d682caac2e1fca14d83b6fac3899829856f62fe81f45dc578c7c70"
    "main pool/main/h/harfbuzz/libharfbuzz-subset0_10.2.0-1+deb13u1_armhf.deb 947a552204dcccba4605f244477045973ab2e52e6f1bea788999754935e801be"
    "main pool/main/h/harfbuzz/libharfbuzz0b_10.2.0-1+deb13u1_armhf.deb f3fb46b3053bd7d0f4dccab3dab5658761e1fa33290a5778e622791d4ce321f5"
    "main pool/main/n/nettle/libhogweed6t64_3.10.1-1_armhf.deb 1c9ff2e23a6050f1d192651c5fd1241613ac2121c0ab38eb94800dc827a88e01"
    "main pool/main/libi/libice/libice6_1.1.1-1_armhf.deb f3151383df4169d49f4f59de913b70f4ea752de41c5a756212c5f177283f1654"
    "main pool/main/libi/libidn2/libidn2-0_2.3.8-2_armhf.deb 4ac491ae50ca935ee1517fe6860f82b66bf0c5b520a7fea46224fa7329281247"
    "main pool/main/libj/libjpeg-turbo/libjpeg62-turbo_2.1.5-4_armhf.deb 77eb9383b6b824bce2fa4dcf9856963dcb99534d01021708196dfa5dd2f2ec86"
    "main pool/main/k/krb5/libk5crypto3_1.21.3-5+deb13u1_armhf.deb 87662cf065fc7f4d97c8e6bba624e27ebb74dc4b019cb6060be2a36c3f8fa1ec"
    "main pool/main/k/keyutils/libkeyutils1_1.6.3-6_armhf.deb e9b0cdf27e1e85c1531220c1e5d93caac86d997b70f7e2ac63184c96c1184107"
    "main pool/main/k/krb5/libkrb5-3_1.21.3-5+deb13u1_armhf.deb 988151350dfb736c7bb461ffb24bf136aeae72842838270f44d482b7f70d8167"
    "main pool/main/k/krb5/libkrb5support0_1.21.3-5+deb13u1_armhf.deb 10a796a3a4ed1891ef7dbe417f6c435f95214f9916a6ea2e78aaefe87f5fe8cf"
    "main pool/main/l/lcms2/liblcms2-2_2.16-2+deb13u2_armhf.deb d440001647a7151586bcff90ac727ab40d2f99415807521b727744beed4a01e7"
    "main pool/main/z/zlib/libminizip1t64_1.3.dfsg+really1.3.1-1+b1_armhf.deb 9b274781c8abbc321e0dadaa3b21d17b063e1aa4cbd74644ca53df70a5ecd22a"
    "main pool/main/u/util-linux/libmount1_2.41.5-0+deb13u1_armhf.deb bdf34cbc99cf1bb07f1b64e70ddd1c9d6d80e399415eba7864b89f117eba726d"
    "main pool/main/l/lame/libmp3lame0_3.100-6+b3_armhf.deb 02ed502f7d6dc79276be6d204f6ea2ff32078bb963f1eef2d0234d606c53b460"
    "main pool/main/m/mpg123/libmpg123-0t64_1.32.10-1+deb13u1_armhf.deb 61ce46da20a4ecf3e5c70b43560c96b5a08c00f6315b7df5962c390734190bdf"
    "main pool/main/n/nettle/libnettle8t64_3.10.1-1_armhf.deb e81e48d88828d333a4a074073eeb46b38547db40a62a9da167dd82e2cbf23708"
    "main pool/main/n/nspr/libnspr4_4.36-1_armhf.deb 0c137e0fe017c5e0beaab592e6cf9d25181ed7363101b6d0ecb7b1f2384e913b"
    "main pool/main/n/nss/libnss3_3.110-1+deb13u4_armhf.deb b885665fdd796320df2ae5fa9b2a2432a4f9734d76baef16739a60624abff67b"
    "main pool/main/libo/libogg/libogg0_1.3.5-3+b2_armhf.deb 2c74bc6d76ea135e436cb340e8b6f168f4ad9cdb26db7a3e3de4f4bb18daaaa7"
    "main pool/main/o/openh264/libopenh264-8_2.6.0+dfsg-2_armhf.deb 624a6b40ac3c756c5bf54389035d98fd263ee72a5441e69cec22509b893173f0"
    "main pool/main/o/openjpeg2/libopenjp2-7_2.5.3-2.1~deb13u2_armhf.deb cb4468712c4399d7849b0d389729984720db8a0a019ee2292c32cdc2e6eea3d0"
    "main pool/main/o/opus/libopus0_1.5.2-2_armhf.deb 81df1a359f27c434d62024f3a8a1effdcf196c1dd9cc57bc39c70adf7e5881ed"
    "main pool/main/p/p11-kit/libp11-kit0_0.25.5-3_armhf.deb d2077b09d116f2f281da249c185ddcc717c6a72d9fafaca1b2d42c7fa97ae2f6"
    "main pool/main/p/pango1.0/libpango-1.0-0_1.56.3-1_armhf.deb 3692faf0b166eebe6a64238338d9be5f58416ec4c08f9abedde06c1a40885b6a"
    "security pool/updates/main/p/pcre2/libpcre2-8-0_10.46-1~deb13u3_armhf.deb 4bba1651d061cb205552b987a82915ed3d0d4cacfe14f7eea1ef925be9aec094"
    "main pool/main/p/pixman/libpixman-1-0_0.44.0-3_armhf.deb e1f0da1b865cb0944136c782dd6e2a19eb2d0976235a07f18ae8156dc51d3289"
    "security pool/updates/main/libp/libpng1.6/libpng16-16t64_1.6.48-1+deb13u6_armhf.deb e86c75e5c5c8ec8c999228407ba200f480ab3465a7c97fffaafca9bedb0dbae5"
    "main pool/main/p/pulseaudio/libpulse0_17.0+dfsg1-2+b1_armhf.deb 2d9651023010f8593f1c1b2cac5be001a3e9bb89e6aae8e513cd424f0cd25e23"
    "main pool/main/libs/libselinux/libselinux1_3.8.1-1_armhf.deb 9b58d1e5608466c90d3cf948b0dce2b1436d8f1b0f585937e9665b3227f2175a"
    "main pool/main/libs/libsm/libsm6_1.2.6-1_armhf.deb 428ab5f7dfb88aa9ccf94a5559826e1c2c01742f66a18af5b9a4323e98d15806"
    "main pool/main/libs/libsndfile/libsndfile1_1.2.2-2+deb13u1_armhf.deb 7f40a3ad9a6d58c818076ac2db6da1b1601fa9eb02071e49548b60a22b515e72"
    "main pool/main/s/sqlite3/libsqlite3-0_3.46.1-7+deb13u2_armhf.deb 15d6a91264010e8ee439d0b3e95782dd87e31fea71aee379deca53fbb99ea957"
    "security pool/updates/main/o/openssl/libssl3t64_3.5.7-1~deb13u3_armhf.deb be99a741119db2fcec44ac2cead4e9f6143ffa1a37e0c667cec99b425641f223"
    "main pool/main/g/gcc-14/libstdc++6_14.2.0-19_armhf.deb 9c82eecc30961a3da3e062c0dba8ce076736059f4b8e7794c803985e75aea48b"
    "main pool/main/s/systemd/libsystemd0_257.13-1~deb13u1_armhf.deb 4f16f1d40fc759cd7bf40295a3515d5ea11273aac10ab1da96701b0e36adf645"
    "main pool/main/libt/libtasn1-6/libtasn1-6_4.20.0-2+deb13u1_armhf.deb f65d32ac8c9f3aea73f960100c9590b7884e4d4569fe6d02d67ef87a55004ad2"
    "main pool/main/libt/libthai/libthai-data_0.1.29-2_all.deb fd38d40602834d510a29140bd27fd48485105e834f03dccab1c02e2edaa794dd"
    "main pool/main/libt/libthai/libthai0_0.1.29-2+b1_armhf.deb 5f139cabbf31c1a9f6f229836b1c8648c721c45af41f495955a0d3ffa98b1b5f"
    "main pool/main/s/systemd/libudev1_257.13-1~deb13u1_armhf.deb 11dfc3d8f63cecba13e526d4d129a5435a15d6aba5a9aa9f5f8a6d877ec866cf"
    "main pool/main/libu/libunistring/libunistring5_1.3-2_armhf.deb 253b83fbee9a5d7980cb5cb4f8557b1f0b2f5b906afd36c1136d2c63459167aa"
    "main pool/main/u/util-linux/libuuid1_2.41.5-0+deb13u1_armhf.deb a33526c90db21b85b700d72a979dac8c515449fe0f621b0bf76ed112aa60a397"
    "main pool/main/libv/libvorbis/libvorbis0a_1.3.7-3_armhf.deb b4bf67f3a196e37e1fcc4ae98780ae6bdb152acc1901a704d75e0d01b264f73d"
    "main pool/main/libv/libvorbis/libvorbisenc2_1.3.7-3_armhf.deb 63b574c8fa7bfd3a4d8b943c269701896f17e82c322a750d9f5849e0c8ade42b"
    "main pool/main/w/wayland/libwayland-server0_1.23.1-3_armhf.deb 6c9b4363cb89833bd9828e94c1021ebbb5b3a16aeb8b55165ef017888755a5d1"
    "main pool/main/libx/libx11/libx11-6_1.8.12-1_armhf.deb 089be6f36bc43a1d37c3f9f02a16e71bfb874269cb1928cb4febb396c4917170"
    "main pool/main/libx/libx11/libx11-data_1.8.12-1_all.deb c54f87069888f80ba4da586da6147d74c7598ccdd8b90906dbc4271fa414c738"
    "main pool/main/libx/libx11/libx11-xcb1_1.8.12-1_armhf.deb 30a53e8d2502d91592143650ce19877c96c1065e5a995072c501e7fcc0b5509c"
    "main pool/main/libx/libxau/libxau6_1.0.11-1_armhf.deb 1737feac1d2c51e0900006f5e74788ec6f30b96959b90b35f63c01d7a22cd4ba"
    "main pool/main/libx/libxaw/libxaw7_1.0.16-1_armhf.deb a73491fb3cd07b7d54c0e22d4658262a5349c82b044278df26e1cdb4d29750f0"
    "main pool/main/libx/libxcb/libxcb-render0_1.17.0-2+b1_armhf.deb 709c1685423e28f16a19c4cff4fcf3720ea0966b192651e810258acc3e691840"
    "main pool/main/libx/libxcb/libxcb-shape0_1.17.0-2+b1_armhf.deb 9c544f0767bc498a30366cd303e7bebbc31228199a52f6052f7e3dcdec75e3bd"
    "main pool/main/libx/libxcb/libxcb-shm0_1.17.0-2+b1_armhf.deb bffae60442256cb2bab198933ff53643a0b27beb7b9f8f567ab8fd6842bd57b0"
    "main pool/main/libx/libxcb/libxcb1_1.17.0-2+b1_armhf.deb 2b82ee239a1429ceca8a6442fdda9739ee1e423fbcfcf74682a62ffd90a8e985"
    "main pool/main/libx/libxcomposite/libxcomposite1_0.4.6-1_armhf.deb 6d71fa691b24db811d6fb8a866d9bef65490ac1017606d7d22f05c38f60ef5dc"
    "main pool/main/libx/libxdamage/libxdamage1_1.1.6-1+b2_armhf.deb f37ae1dafc7f7de3dc88d483f913cda8bb4e7e7be43faa5860095112b064ee47"
    "main pool/main/libx/libxdmcp/libxdmcp6_1.1.5-1_armhf.deb e801eb04b7eecd257096a612985646588b578e715012cdc929b6c955ccc40836"
    "main pool/main/libx/libxext/libxext6_1.3.4-1+b3_armhf.deb 7af32ae48512aa664664ab4a1c1669d8d7ccdbb1f8416627799723c8517f43c8"
    "main pool/main/libx/libxfixes/libxfixes3_6.0.0-2+b4_armhf.deb 0286281d056af6f4e17c40873aad4daac77615a51c78f94c123880aac8919a45"
    "main pool/main/x/xft/libxft2_2.3.6-1+b4_armhf.deb c929a35e37d6780cbdc816e30862a224a59b9ea9dd56bcc84a999a785a4eff79"
    "main pool/main/libx/libxi/libxi6_1.8.2-1_armhf.deb 05327880eb3a1b9f006e5d7e118a394c8efc3fc942a14b334ef276caf2acea09"
    "main pool/main/libx/libxinerama/libxinerama1_1.1.4-3+b4_armhf.deb 2720461829d7f41c2c1c47d918b32a4ae2978264905dc6a2a03d72f45d496c06"
    "main pool/main/libx/libxkbcommon/libxkbcommon0_1.7.0-2_armhf.deb 201106ed6322ad28fd4d4a98e62b07443403c5811abc23b8d384605749b45764"
    "main pool/main/libx/libxkbfile/libxkbfile1_1.1.0-1+b4_armhf.deb 6d1c2fe86b103de48ce3218909d7fcf91fad3346c45f225dfa5712d43ee5866c"
    "main pool/main/libx/libxmu/libxmu6_1.1.3-3+b4_armhf.deb d2b7ccd6e11509d1ac9b0d160015e38a905944e9f55e5844bac81964044fd16d"
    "main pool/main/libx/libxmu/libxmuu1_1.1.3-3+b4_armhf.deb 5e4e18768671e2ba9583b45bac8df4936a3fcb219ef14a66e9dfadd0a28959b8"
    "main pool/main/libx/libxnvctrl/libxnvctrl0_535.171.04-1+b2_armhf.deb 63b49c7519ea0d4dbdc9b3b7679535993f97d007024c3178990fc8910e3c3750"
    "main pool/main/libx/libxpm/libxpm4_3.5.17-1+deb13u1_armhf.deb 0f54bc6378166479661e7b364b9ca5996006a69982acaff3eb0f8203d891dd22"
    "main pool/main/libx/libxrandr/libxrandr2_1.5.4-1+b3_armhf.deb 2ac65a673bc54ff7532a5aacb15d9d8759dca7db7de56b90c52e465746602422"
    "main pool/main/libx/libxrender/libxrender1_0.9.12-1_armhf.deb 350cd7a3b856324787ffc7cb0038220d2dc3f2375fb704df70fb9e8adeaad36b"
    "main pool/main/libx/libxt/libxt6t64_1.2.1-1.2+b2_armhf.deb 14aeb5d31cb1a5698640afc6e3f4b0bdc89f2e9b0c9c12afb51125de1bb406c1"
    "main pool/main/libx/libxtst/libxtst6_1.2.5-1_armhf.deb 0ad7d50189f6d44cf14512e04771bc373fdc597104b6ca5e78a96b5a506658c5"
    "main pool/main/libx/libxv/libxv1_1.0.11-1.1+b3_armhf.deb e62882508ba32b77f869f969b64aa287008fe1dfae67bc9b3d74ed3e55d861fe"
    "main pool/main/libx/libxxf86dga/libxxf86dga1_1.1.5-1+b3_armhf.deb aec4ede4184812c6c3baef716c165b6a7a54e51c7b2c2785ed132157e1b2ab3a"
    "main pool/main/libx/libxxf86vm/libxxf86vm1_1.1.4-1+b4_armhf.deb c3d1d9512b7f722c100ae17ea295cc6e2f608203a21b9054959638e92b8832e2"
    "main pool/main/libz/libzstd/libzstd1_1.5.7+dfsg-1_armhf.deb da5238dd84fc51f782f39d435821bff556409b3dbc82d232e4e81f427fb1ca65"
    "security pool/updates/main/o/openssl/openssl-provider-legacy_3.5.7-1~deb13u3_armhf.deb 4dfd45ef1125d9aad8a78a23b9ab73ec8ba1ee103ef55c9c56befe3bca39787e"
    "main pool/main/x/x11-utils/x11-utils_7.7+7_armhf.deb a6d8335e844e4d0d85033804ed111cec6ed47af9c21d43ca8deafdc6b0ddada3"
    "main pool/main/x/xdg-utils/xdg-utils_1.2.1-2_all.deb 01dd31db093f1e810824519200112ec3fd447ba0341f7213244525eaa289355e"
    "main pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_armhf.deb 81c55a59e1570477ecef6a449bf6dce44dad67ba4ce9e04760451d4cfe200534"
)

for tool in curl sha256sum dpkg-deb readelf mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-chromium-armhf: $tool is not installed" >&2; exit 1; }
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
        || { echo "fetch-chromium-armhf: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

mkdir -p "$out/pool"
tree="$out/tree"
rm -rf "$tree"
mkdir -p "$tree"

for entry in "${debs[@]}"; do
    read -r archive path sum <<< "$entry"
    case $archive in
        security) base=$security ;;
        *) base=$debian ;;
    esac
    dpkg-deb -x "$(fetch "$path" "$base" "$sum")" "$tree"
done

# Documentation and manuals, which nothing runs.
rm -rf "$tree/usr/share/doc" "$tree/usr/share/man" "$tree/usr/share/lintian"

lib="$tree/usr/lib/arm-linux-gnueabihf"
test -x "$tree/usr/lib/chromium/chromium" \
    || { echo "fetch-chromium-armhf: no chromium in the tree" >&2; exit 1; }
test -e "$tree/usr/lib/ld-linux-armhf.so.3" \
    || { echo "fetch-chromium-armhf: no loader at usr/lib" >&2; exit 1; }

# Every library an ELF file on the volume needs must be on it: in glibc's
# directory, beside Chromium, the loader, or in `pulseaudio/`, where
# libpulse's RUNPATH finds its common library.
missing=0
while IFS= read -r -d '' file; do
    head -c 4 "$file" | grep -q $'\x7fELF' || continue
    for needed in $(readelf -d "$file" 2> /dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
        if [ ! -e "$lib/$needed" ] \
            && [ ! -e "$tree/usr/lib/chromium/$needed" ] \
            && [ ! -e "$lib/pulseaudio/$needed" ] \
            && [ ! -e "$tree/usr/lib/$needed" ]; then
            echo "fetch-chromium-armhf: ${file#"$tree"/} needs $needed, which is not on the volume" >&2
            missing=1
        fi
    done
done < <(find "$tree/usr/lib/chromium" "$lib" -maxdepth 1 -type f -print0)
[ "$missing" = 0 ] || exit 1

# Room for what Chromium writes beside itself, and a size that does not
# depend on how mkfs rounds: small, for the reason at the top.
size=$(( $(du -sm "$tree" | cut -f1) + ${FERRIX_CHROMIUM_ROOM:-64} ))
image="$out/chromium.img"
rm -f "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
# The tree stays beside the image, so the same files can be looked at here.
echo "chromium volume: $image (${size} MiB), unpacked in $tree"
