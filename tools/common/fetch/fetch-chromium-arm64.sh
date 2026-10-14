#!/usr/bin/env bash
# Fetch Debian's Chromium for arm64 and every library it loads, and put them
# on a btrfs volume: the disk Chromium runs from on an AArch64 Ferrix -- the
# Pixel 7's VM above all (docs/CHROME.md §11).
#
# Why not Google's build: Chrome for Testing publishes linux64 alone, and
# tools/common/fetch/fetch-chrome.sh's volume is that. Debian 13's security archive
# carries Chromium 154.0.8037.57 for arm64, the very version of Chrome for
# Testing the x86-64 volume holds, built by Debian with its own libraries.
#
# The package list is Chromium's dependency closure as
# `tools/common/gen/debian-closure.py --arch arm64 chromium --skip ...` resolved it
# from trixie, trixie-updates and trixie-security on 2026-09-26, leaving out
# what no Ferrix volume runs -- debconf, systemd and D-Bus's daemons, GTK
# (which Chromium only `dlopen`s, for its file dialog and theme) and Mesa's GL
# drivers and LLVM (Chromium draws in software here, with `--disable-gpu`).
# Every download is pinned by the SHA-256 its index gave. After unpacking,
# every library each ELF file on the volume needs is looked for on it, so a
# library the closure left out fails here rather than on Ferrix.
#
# The volume is a Debian-shaped tree at its root -- `usr/lib`,
# `usr/lib/aarch64-linux-gnu`, Chromium in `usr/lib/chromium` -- which Ferrix
# mounts at `/data`; the image links the directories glibc names into it.
#
# Writes $FERRIX_CHROMIUM_VOLUME/chromium.img (default
# ~/.local/share/ferrix/chromium-arm64/chromium.img). Needs curl, sha256sum,
# dpkg-deb, readelf and mkfs.btrfs; no root.
#
# Usage: tools/common/fetch/fetch-chromium-arm64.sh

set -euo pipefail

debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
security=${DEBIAN_SECURITY_MIRROR:-https://security.debian.org/debian-security}
out=${FERRIX_CHROMIUM_VOLUME:-$HOME/.local/share/ferrix/chromium-arm64}
# Where a pinned file comes from once the mirror has dropped it, as
# trixie-security drops Chromium's update when the next one comes:
# snapshot.debian.org's archive as it was the day the closure was resolved.
# The SHA-256 pins are checked either way.
snapshot=${DEBIAN_SNAPSHOT:-https://snapshot.debian.org/archive}
snapshot_time=20260926T235959Z
# The downloads, kept between runs; FERRIX_CHROMIUM_POOL names another
# directory, an empty one to fetch everything as a new machine does.
pool=${FERRIX_CHROMIUM_POOL:-$out/pool}

# `<archive> <pool path> <sha256>`: main and updates come from $debian,
# security from $security.
debs=(
    "main pool/main/a/at-spi2-core/at-spi2-common_2.56.2-1+deb13u2_all.deb a2a7b53c9925ec33a471d56dd60f5effe35cece109ec4296cc8d98a1250213b4"
    "security pool/updates/main/c/chromium/chromium_154.0.8037.57-1~deb13u1_arm64.deb df210b1e57d974003c50f0c38298e0841dba63053b392653aa60bd34d168bed0"
    "security pool/updates/main/c/chromium/chromium-common_154.0.8037.57-1~deb13u1_arm64.deb 5eecb71dd7b23e7c21dd8f1c2c68f2cf80e2f802486b7c59f5286f7a28544be6"
    "main pool/main/f/fontconfig/fontconfig_2.15.0-2.3_arm64.deb f899e800435bc3bd3ecc6139bfcb2b45b3dbd2ffa59c4b9b3cc246e9661052af"
    "main pool/main/f/fontconfig/fontconfig-config_2.15.0-2.3_arm64.deb 10f643f467b51cf25fc18d1945bc61246dd8d8d1fe6bc712a2b1712c74ebc4d6"
    "main pool/main/f/fonts-dejavu/fonts-dejavu-core_2.37-8_all.deb 86635b3d25b3655fc11cb3ecc3af59f0bf19643b02b94f2de48bd10253cdba12"
    "main pool/main/f/fonts-dejavu/fonts-dejavu-mono_2.37-8_all.deb 3003e98a5debfdeadc7040a7f715fe9fe6fb67f68deacf6049b54e30f07fc014"
    "main pool/main/g/gcc-14/gcc-14-base_14.2.0-19_arm64.deb 34ee90679b018c0e64234747a4c4c0ae6b7f63541115037465a8627c2dfbc594"
    "main pool/main/a/alsa-lib/libasound2-data_1.2.14-1+deb13u1_all.deb 04688afdff3769c0f685541daed7b2f6f0cb946799ddf1d5847ddf11fe245559"
    "main pool/main/a/alsa-lib/libasound2t64_1.2.14-1+deb13u1_arm64.deb 9c808d819ccddc37c793dd3186c2ecd858b2feb2474cebb883f70386e7890d39"
    "main pool/main/liba/libasyncns/libasyncns0_0.8-6+b5_arm64.deb 818bbcaa7ada536a1bdd5b1c26b77608606403f5c15557691e83d0b4433b89ef"
    "main pool/main/a/at-spi2-core/libatk-bridge2.0-0t64_2.56.2-1+deb13u2_arm64.deb b86030826a7f81a3a2a8442d58a8a61a7d01376a97ba62c1f2d410f5e6f1b885"
    "main pool/main/a/at-spi2-core/libatk1.0-0t64_2.56.2-1+deb13u2_arm64.deb c35cd1a2bd7f6750e56a0755571b091a13fdb606354312088626ba639719c953"
    "main pool/main/g/gcc-14/libatomic1_14.2.0-19_arm64.deb 7ccff90234484bf21ae5ecb79d25b6a84b2f4038f65dd139085b2e3a937cd8dc"
    "main pool/main/a/at-spi2-core/libatspi2.0-0t64_2.56.2-1+deb13u2_arm64.deb 0b5266747ef215e161152772081ce3b2990d72d02447185faa1af661e11e2d39"
    "main pool/main/a/avahi/libavahi-client3_0.8-16_arm64.deb aad7c1a4f0e8b9131881ebdd61150bb9e15f11534aafe4877e61fefc266f1586"
    "main pool/main/a/avahi/libavahi-common-data_0.8-16_arm64.deb 75d80488f931f6cc577aaf366b1a17099bd4d0c179dfd33d014f564094255da9"
    "main pool/main/a/avahi/libavahi-common3_0.8-16_arm64.deb 495d3502dbaed9e71c54cd0030fd135d62140b7c780620cc14a0a8a615adda14"
    "main pool/main/u/util-linux/libblkid1_2.41.5-0+deb13u1_arm64.deb d7d122b4c7a0a8ae6135a1687c9c4ce80a88f9e498c01998f634dd32caab9016"
    "main pool/main/b/brotli/libbrotli1_1.1.0-2+b7_arm64.deb 10270398c4842e71c72b4081fa1761728b3225d6c8dce7c58d5baafe58a5e18f"
    "main pool/main/b/bzip2/libbz2-1.0_1.0.8-6_arm64.deb 3537fe4fc577a60c8a9568873cd577db7ebc135eb8bb1825f18caf8972d9d2f2"
    "main pool/main/g/glibc/libc6_2.41-12+deb13u4_arm64.deb 8784eda966b189c777a384dac5ce009e8fc9b52d006926c5a013e7fa8aa688cc"
    "main pool/main/c/cairo/libcairo2_1.18.4-1+b1_arm64.deb de0e190f4566b51942387fb84fe85f064d23da896b8e2080930d53879a11630d"
    "main pool/main/libc/libcap2/libcap2_2.75-10+deb13u1+b3_arm64.deb 863aa5b9cdf6f80010b48982d4639126b09a89eeef2c0c5c7be226297c811581"
    "main pool/main/e/e2fsprogs/libcom-err2_1.47.2-3+b12_arm64.deb 1acba7e14a17a3f4b8008454e1a75840a5cdff4259d3c1d369092d82089ea0d8"
    "main pool/main/c/cups/libcups2t64_2.4.10-3+deb13u2_arm64.deb 7ab4a1f78863f9f4c9fab7e56633e84932c94fb835c8f9e83062988be53e5b5d"
    "main pool/main/libd/libdatrie/libdatrie1_0.2.13-3+b1_arm64.deb d513d0602836c4df782c15dc9472c17ab5a815343aabc9ebe7dc9604c0acfaa0"
    "main pool/main/d/dav1d/libdav1d7_1.5.1-1_arm64.deb bcb430fa4c5f05bc3fa4dd9f18b7b0498ec9701c23e83bf57a29996216c3a245"
    "main pool/main/d/dbus/libdbus-1-3_1.16.2-2_arm64.deb ca6bb5f2047ad58d274cb7d14216035a07d8e5aae6f6a0122c4f6293c9c39515"
    "main pool/main/d/double-conversion/libdouble-conversion3_3.3.1-1_arm64.deb ea827a47ddffeaa8e95dc8d0c3b8aa9c79b33b2be77109450c2b06a2b89644d8"
    "main pool/main/libd/libdrm/libdrm-common_2.4.124-2_all.deb 9a8a6c65c165e9964f106fb4ac710959b5d33e0790227e3ab6b27c4742d1254a"
    "main pool/main/libd/libdrm/libdrm2_2.4.124-2_arm64.deb b535506630bb6a9a616bc07caf3b29915f980d8c3437852fb96ea64fb1a8a3f8"
    "main pool/main/e/expat/libexpat1_2.8.3-1~deb13u1_arm64.deb c3248c4beb721c8290e6e34527ae56b1fc965aa6a14982ca602df4458ef0854e"
    "main pool/main/libf/libffi/libffi8_3.4.8-2_arm64.deb d84a783b818f2386627604e64b793eb4ac5bb9ea1ba321a194a1c9c82fe09a01"
    "main pool/main/f/flac/libflac14_1.5.0+ds-2_arm64.deb 11800827306e56fcbd1a65ea67d9d13126fc02bf87fdfecd35e0f48a145b3c59"
    "main pool/main/f/fontconfig/libfontconfig1_2.15.0-2.3_arm64.deb 872781cf62926b68c34a4302a3caae334ff8d4d83a2b2477f8fb454460aadc99"
    "main pool/main/f/freetype/libfreetype6_2.13.3+dfsg-1+deb13u1_arm64.deb 0c426e83f1af816b9a4df9056fd2238463d1cd07969ee640965da9fb6ecaf107"
    "main pool/main/f/fribidi/libfribidi0_1.0.16-1_arm64.deb 5f5bfdf6ef126b0a38f8ebb8751cbbb845fe2cd4e1dd5258d241be243e09897f"
    "main pool/main/m/mesa/libgbm1_25.0.7-2+deb13u1_arm64.deb 2beb8d20ca715a01b7b2352c1167c0d7097c9d8a486704a9a966f95d4077cdfc"
    "main pool/main/g/gcc-14/libgcc-s1_14.2.0-19_arm64.deb 1108bc87879833d6d9a145f22a4a15cddb34e065b4b5f4b97bee586adbac2851"
    "main pool/main/g/glib2.0/libglib2.0-0t64_2.84.4-3~deb13u5_arm64.deb d446f0ea85fb9dc411219ce861e59e5eabe1981c796918771a7d2aa97cc72481"
    "main pool/main/g/gmp/libgmp10_6.3.0+dfsg-3_arm64.deb a27bbc27f119161ea9702c8dd66f54131cdf0d2ca73000f50ea91ef2fdfef0fb"
    "main pool/main/g/gnutls28/libgnutls30t64_3.8.9-3+deb13u4_arm64.deb 337ef41ab360015d051d6a07f85f8aa6706b35712e3dea23157eba9c902196e2"
    "main pool/main/g/graphite2/libgraphite2-3_1.3.14-2+deb13u1_arm64.deb b0c3414b113de62be1dc28fa2c899ac0856256e2f094d92c36c3275a5053311a"
    "main pool/main/k/krb5/libgssapi-krb5-2_1.21.3-5+deb13u1_arm64.deb 70803e8f5b9dd0a7167671bdd0b3cd05642384c1fc49e8270d340d85bf6c7d4c"
    "main pool/main/h/harfbuzz/libharfbuzz-subset0_10.2.0-1+deb13u1_arm64.deb 94762ffe1961b0d3d042398fef3f56545f99c5e070b17a557782664056b05a00"
    "main pool/main/h/harfbuzz/libharfbuzz0b_10.2.0-1+deb13u1_arm64.deb 3ff928f981b54b491ee12e4cf68558cc5d61c652bdab276cd4c4c3fbbcee16ca"
    "main pool/main/n/nettle/libhogweed6t64_3.10.1-1_arm64.deb cb92d5a51c4fd6c7b7cbb62aaa60c7af830d0bea5b410fb1f47ee685e26944d1"
    "main pool/main/libi/libice/libice6_1.1.1-1_arm64.deb 048874f2296660a5e925844635f4d050cb054e7d3459cb15b4c30afb113fbecb"
    "main pool/main/libi/libidn2/libidn2-0_2.3.8-2_arm64.deb d2e5cef812f15db1eeb35f0a193158ee102a9e94284036c0e293521f2761d2d7"
    "main pool/main/libj/libjpeg-turbo/libjpeg62-turbo_2.1.5-4_arm64.deb e4989073bb0bac8a6ec043c7adb80e1dfe601d8552233da48bb24ab45d1a1d4a"
    "main pool/main/k/krb5/libk5crypto3_1.21.3-5+deb13u1_arm64.deb 2453e8d4fa3b91b868d5bc10e84a15ad9c7bc4e4cb49379cb1f560bdce50db4a"
    "main pool/main/k/keyutils/libkeyutils1_1.6.3-6_arm64.deb 5e680f317a6613161986417fb3ed63007c9228343f5ac2348551b103cc7fd7b4"
    "main pool/main/k/krb5/libkrb5-3_1.21.3-5+deb13u1_arm64.deb 1f5c46480abf894f662d894696c8a6c6510c2b20616cd453e88b27b11d3be25d"
    "main pool/main/k/krb5/libkrb5support0_1.21.3-5+deb13u1_arm64.deb 6381bcb54e58854f9ebac2dfa580c3d6112d01b4ca3f725e7263b9374f79608e"
    "main pool/main/l/lcms2/liblcms2-2_2.16-2+deb13u2_arm64.deb 6636997fc488327a8d0951a038df6e5178a52b3f5e5cc2386d42655621f5672d"
    "main pool/main/z/zlib/libminizip1t64_1.3.dfsg+really1.3.1-1+b1_arm64.deb 46f361d1deb90caa1760f230c9c119fa4ce255636b12e6d817a0f65d6de87aad"
    "main pool/main/u/util-linux/libmount1_2.41.5-0+deb13u1_arm64.deb 14e2d555459f3c0b1442bb49dd2d266ea202a003198c86f65508a1dbfaa18732"
    "main pool/main/l/lame/libmp3lame0_3.100-6+b3_arm64.deb ed3563722129ffa4def03b7af0b589fa20eef972bdcc4599202ddb1ac654c58e"
    "main pool/main/m/mpg123/libmpg123-0t64_1.32.10-1+deb13u1_arm64.deb d284df39ff3b64f1cdf274352613c52534c55eed82c3a2ff0fc7ba154acd7bf0"
    "main pool/main/n/nettle/libnettle8t64_3.10.1-1_arm64.deb 16107ce5a7b522da021fa8aba42d42af9c5e90ddabfcda3710aff6ea95808ee0"
    "main pool/main/n/nspr/libnspr4_4.36-1_arm64.deb b8cd0221e4c35a77ec6e3b89959ca96deb75919a3c55b6735ca0288a1b61bdbb"
    "main pool/main/n/nss/libnss3_3.110-1+deb13u4_arm64.deb 1a9f35c238e8bbb570fbdae88ffe43a3cf3462e1582cbe0903a9aa949e61f3e8"
    "main pool/main/libo/libogg/libogg0_1.3.5-3+b2_arm64.deb d59a83ab352bf0c16ce9513fdf264f87214813ca8c628c652801ef476ece8286"
    "main pool/main/o/openh264/libopenh264-8_2.6.0+dfsg-2_arm64.deb dd40ffeeb006af3a610e015881340c704b8bb38627434a150b7d9e39e657dfbe"
    "main pool/main/o/openjpeg2/libopenjp2-7_2.5.3-2.1~deb13u2_arm64.deb e27f4d03d4b5836473be3210b16567aa9435a337f8c4537868773a123f9c9e57"
    "main pool/main/o/opus/libopus0_1.5.2-2_arm64.deb 1d980e8a4717074805a98dbe07671cb3bee068c25709315f3de836980c629836"
    "main pool/main/p/p11-kit/libp11-kit0_0.25.5-3_arm64.deb 78bfc746e68a5217e845f488f69100ed89d10c8886fbd3ffce937acb6a7d88fb"
    "main pool/main/p/pango1.0/libpango-1.0-0_1.56.3-1_arm64.deb 5c5c6061afdff84a76418c9c2cf8bd4f3b6174aa7bf39ddf58bfa7702a59d3f2"
    "main pool/main/p/pcre2/libpcre2-8-0_10.46-1~deb13u2_arm64.deb e7d2c997dac145c16457be0fed3d084c98cd030bec7633eec7e5bde6dbb97712"
    "main pool/main/p/pixman/libpixman-1-0_0.44.0-3_arm64.deb b502cd8b1ece222f42b44c42c33a300d84152c20089660167b9422ad2ebb6132"
    "main pool/main/libp/libpng1.6/libpng16-16t64_1.6.48-1+deb13u5_arm64.deb 11096ad43504ca24e8044bcc89e922d932a2812ff25568613b28d22235190ba5"
    "main pool/main/p/pulseaudio/libpulse0_17.0+dfsg1-2+b1_arm64.deb 2de5610e3affe69cfb11110f890375261b47c9e0fc1a81eef7ecc4d6d2dc9a6d"
    "main pool/main/libs/libselinux/libselinux1_3.8.1-1_arm64.deb fc13da4c7783b8f4361d92ef1045931f298d1b0ac78ed1a85c2232615f49fc6f"
    "main pool/main/libs/libsm/libsm6_1.2.6-1_arm64.deb 4906619bbec923da6808cb7e4e817a7667d13cd43bc7d98c77702a03bb103f42"
    "main pool/main/libs/libsndfile/libsndfile1_1.2.2-2+deb13u1_arm64.deb 6c94f17e1f02778f98b96bb9ae19ff30085662e96685898a018c26bfd4bfc3f2"
    "main pool/main/s/sqlite3/libsqlite3-0_3.46.1-7+deb13u2_arm64.deb 5b09efca71cb7a16d67f5453af3989ae1a84fe6b68c6dd3913de527cb0ca23aa"
    "main pool/main/o/openssl/libssl3t64_3.5.7-1~deb13u2_arm64.deb ec131326aa9fa9ec934eca386bc7991f328fe383eaefd3e43bd8901a9199c5ae"
    "main pool/main/g/gcc-14/libstdc++6_14.2.0-19_arm64.deb 6669b0c52a2e7c6af9adfdabce3ff6e286065cdfbc7b85280862b5f799daebee"
    "main pool/main/s/systemd/libsystemd0_257.13-1~deb13u1_arm64.deb d2f63c408549c29eaefb6f4c948393edce99f45ee8e43c991391bef63fb7d54b"
    "main pool/main/libt/libtasn1-6/libtasn1-6_4.20.0-2+deb13u1_arm64.deb e421da949cd26245f24c594265d6a01900b5e060e80793f68b977f7bfaa009ef"
    "main pool/main/libt/libthai/libthai-data_0.1.29-2_all.deb fd38d40602834d510a29140bd27fd48485105e834f03dccab1c02e2edaa794dd"
    "main pool/main/libt/libthai/libthai0_0.1.29-2+b1_arm64.deb 4ea5b9d09449023a30e4d5237b8a69e19d6b3e576e36570818c87a093e2b5eec"
    "main pool/main/s/systemd/libudev1_257.13-1~deb13u1_arm64.deb 459b1c9c17cc3c586e90d13d2a991715d498c6eed6c046cabf321021167bc380"
    "main pool/main/libu/libunistring/libunistring5_1.3-2_arm64.deb 4847467a0e47039837895e88f5a99a4a86a3d8266be40c3c896fc7c10d552dc7"
    "main pool/main/u/util-linux/libuuid1_2.41.5-0+deb13u1_arm64.deb 448ad6b190184dbff24508ef2eda88bd737a5ffc0f9750eb779fcf9885292bea"
    "main pool/main/libv/libvorbis/libvorbis0a_1.3.7-3_arm64.deb 6e8e64ebe692dd2d019af7f914f2e3b81bf13d4b1ce2516fb4248a0310ef7cf6"
    "main pool/main/libv/libvorbis/libvorbisenc2_1.3.7-3_arm64.deb 4a477b3ae19ba50a288f036c8176d61cd9f3836ba65472f88b95e3f6b3bf2dd3"
    "main pool/main/w/wayland/libwayland-server0_1.23.1-3_arm64.deb 062702d57cf07b42ab7f0fa1aac8bc3f605eb47f2f0b9ecb1614674140d79f1b"
    "main pool/main/libx/libx11/libx11-6_1.8.12-1_arm64.deb 646f2d3f2165c8eebceb7f4aaca31e97a818bb11a045964833a9f764b738bac2"
    "main pool/main/libx/libx11/libx11-data_1.8.12-1_all.deb c54f87069888f80ba4da586da6147d74c7598ccdd8b90906dbc4271fa414c738"
    "main pool/main/libx/libx11/libx11-xcb1_1.8.12-1_arm64.deb 1eafaa4295e129d6a81e742ab8da43194daa6b916199923c8cef8ad358985894"
    "main pool/main/libx/libxau/libxau6_1.0.11-1_arm64.deb ac1061728670f4626adaa1288953a0e6fb801c9cae72ee1c3231e63e2609d23a"
    "main pool/main/libx/libxaw/libxaw7_1.0.16-1_arm64.deb 93a4df735b3986c3688bb24fe05add09eba807fca2d885ab280e48d093699337"
    "main pool/main/libx/libxcb/libxcb-render0_1.17.0-2+b1_arm64.deb 7d6faf0afad0b27a047bbf320bf80999e94b7e6a0f73e42069e8b08958910466"
    "main pool/main/libx/libxcb/libxcb-shape0_1.17.0-2+b1_arm64.deb 49f24031f68b9188524954fd1cc237d7cbfe9392d91624cef88d06be13c8075f"
    "main pool/main/libx/libxcb/libxcb-shm0_1.17.0-2+b1_arm64.deb e9e871dc5d2f9265eb5d3028f0d0545cb6f8b748a68997b4c43f5a9416c7ead7"
    "main pool/main/libx/libxcb/libxcb1_1.17.0-2+b1_arm64.deb d0178198e80ed4cacdececabe2c112ec88c7a9258cc11a55b8e267ab14a90d82"
    "main pool/main/libx/libxcomposite/libxcomposite1_0.4.6-1_arm64.deb 0fa60d7733d6354c94cea0b0b8e772a2e317db7040dee4ac84df5d844918a79c"
    "main pool/main/libx/libxdamage/libxdamage1_1.1.6-1+b2_arm64.deb 7803f925e0940b720594f47bab4bc55bea1bdf6f2a3216442c78e53a77f4e19a"
    "main pool/main/libx/libxdmcp/libxdmcp6_1.1.5-1_arm64.deb e10bbb0802181992ecf091e9425171850eee16068729c109214ba4924f81fb52"
    "main pool/main/libx/libxext/libxext6_1.3.4-1+b3_arm64.deb 27cf208c6d2924b22ed3b9ceff304b992e1f07ef7b0ef00239585595581dac99"
    "main pool/main/libx/libxfixes/libxfixes3_6.0.0-2+b4_arm64.deb d29134cbb9230a50ba1d097743d4d4fcf7826c6b48a2d8a303a0e3fc20b837e6"
    "main pool/main/x/xft/libxft2_2.3.6-1+b4_arm64.deb c8053babb508229092aa98e80d9efb58eb157fe92781476edf5be2e21891c368"
    "main pool/main/libx/libxi/libxi6_1.8.2-1_arm64.deb b7148d371c908fa81bf45153fdcb3e7d178fe11e15f9907e29ce6e9ed9aaad24"
    "main pool/main/libx/libxinerama/libxinerama1_1.1.4-3+b4_arm64.deb 780e97f2d0a3f399eeecd616bbadeb5e4417875f271994dc4eaee7bbf9a2239e"
    "main pool/main/libx/libxkbcommon/libxkbcommon0_1.7.0-2_arm64.deb 866888d3cfeb32388dd88a2615c4f78ebf8dd7b272821a83494cba260d8fa7d1"
    "main pool/main/libx/libxkbfile/libxkbfile1_1.1.0-1+b4_arm64.deb b63a122db743b338f65c9d50a15185aa1595f74cdcd42d70e61b48dcf6fd10ce"
    "main pool/main/libx/libxmu/libxmu6_1.1.3-3+b4_arm64.deb 5b43d4b7bd1df59a58503f7a4dfa3eb1e3833d2db0ab8c273a251e7be34a6fe7"
    "main pool/main/libx/libxmu/libxmuu1_1.1.3-3+b4_arm64.deb 522cb4c2029ce7718dc7f00b53a1c303ab98580a37244ccb2d177999a5b9e4af"
    "main pool/main/libx/libxnvctrl/libxnvctrl0_535.171.04-1+b2_arm64.deb 87a7c8e67630667d4b6067f956ab62211f26dee2005560cb6090df22b15b25c7"
    "main pool/main/libx/libxpm/libxpm4_3.5.17-1+deb13u1_arm64.deb 7af00645e130ee865951795dee7b504e82f59e871ec1c6e255904edcb8ed266f"
    "main pool/main/libx/libxrandr/libxrandr2_1.5.4-1+b3_arm64.deb 607a23d6d2cb920e000b24870531a86821cf4e141a4eb0bf38212b47b892d597"
    "main pool/main/libx/libxrender/libxrender1_0.9.12-1_arm64.deb 28396c96e460288d5851913121b053609f7ff34f996d78d7a38c1d2b6ab9f006"
    "main pool/main/libx/libxt/libxt6t64_1.2.1-1.2+b2_arm64.deb 5890026a20d10d0e3c031b93d0b831d398b38f66f15f351035c802beaa6d726a"
    "main pool/main/libx/libxtst/libxtst6_1.2.5-1_arm64.deb 55325d4bbd1f7a7363ef5c5d150603a3d955cb2c307e33c1e5faaa0c92394212"
    "main pool/main/libx/libxv/libxv1_1.0.11-1.1+b3_arm64.deb 2f03e7ab18a58c0a85f22b6b12b51fcf7c376abae6d270ea7b8d7a9bf450fec2"
    "main pool/main/libx/libxxf86dga/libxxf86dga1_1.1.5-1+b3_arm64.deb 2ef26def620be883aa7b3dff5b293abda194cae0986419a911b935b24439831c"
    "main pool/main/libx/libxxf86vm/libxxf86vm1_1.1.4-1+b4_arm64.deb 9ed4bb25e311486207eeaa609decc3905aa9071c4c7f2a605829e3a79ec88b83"
    "main pool/main/libz/libzstd/libzstd1_1.5.7+dfsg-1_arm64.deb 924540bd59fdbfa77a0604360efdaca54411a43daf11c7e002a3c64791b67448"
    "main pool/main/o/openssl/openssl-provider-legacy_3.5.7-1~deb13u2_arm64.deb 7628954796eb651fc499ad98d4e51435691c2ccce0b5fbae6b198cbece742db4"
    "main pool/main/x/x11-utils/x11-utils_7.7+7_arm64.deb 7a072cd6d1247c3c1f6d12bcc8816ea6e074dfda70a10fc1c9028fab60096fca"
    "main pool/main/x/xdg-utils/xdg-utils_1.2.1-2_all.deb 01dd31db093f1e810824519200112ec3fd447ba0341f7213244525eaa289355e"
    "main pool/main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_arm64.deb 209aa5cf671e97b9eb0410844fa6df4cae2e75b0c72e7802ab6c8ece13e6ddef"
)

for tool in curl sha256sum dpkg-deb readelf mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-chromium-arm64: $tool is not installed" >&2; exit 1; }
done

# Download $2/$1 into the pool unless it is there, from the snapshot when $2
# no longer has it, and check it against $3.
fetch() {
    local path=$1 base=$2 sum=$3
    local file="$pool/${path##*/}"
    if [ ! -f "$file" ]; then
        if ! curl -fsSL -o "$file.part" "$base/$path"; then
            local archive=debian
            [[ $path == pool/updates/* ]] && archive=debian-security
            echo "fetch-chromium-arm64: ${path##*/} is not at $base; from $snapshot/$archive/$snapshot_time" >&2
            curl -fsSL -o "$file.part" "$snapshot/$archive/$snapshot_time/$path"
        fi
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { echo "fetch-chromium-arm64: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

mkdir -p "$pool"
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

lib="$tree/usr/lib/aarch64-linux-gnu"
test -x "$tree/usr/lib/chromium/chromium" \
    || { echo "fetch-chromium-arm64: no chromium in the tree" >&2; exit 1; }
test -e "$tree/usr/lib/ld-linux-aarch64.so.1" \
    || { echo "fetch-chromium-arm64: no loader at usr/lib" >&2; exit 1; }

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
            echo "fetch-chromium-arm64: ${file#"$tree"/} needs $needed, which is not on the volume" >&2
            missing=1
        fi
    done
done < <(find "$tree/usr/lib/chromium" "$lib" -maxdepth 1 -type f -print0)
[ "$missing" = 0 ] || exit 1

# Room for Chromium's profile and caches, and a size that does not depend on
# how mkfs rounds.
size=$(( $(du -sm "$tree" | cut -f1) + 512 ))
image="$out/chromium.img"
rm -f "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
# The tree stays beside the image, so the same files can be looked at here.
echo "chromium volume: $image (${size} MiB), unpacked in $tree"
