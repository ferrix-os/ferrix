#!/usr/bin/env bash
# Fetch Google's prebuilt headless Chrome and the Debian libraries it loads,
# and put them on a btrfs volume: the disk `cargo xtask test-chrome` runs
# Chrome from (docs/CHROME.md).
#
# Nothing here is built. `chrome-headless-shell` is Chrome for Testing's
# linux64 build, a 198 MB position-independent glibc program with its own
# ANGLE and SwiftShader beside it. It loads forty libraries of the system's
# -- glib, NSS, D-Bus, the X11 client libraries, gbm, udev, ALSA and what
# those load -- which are Debian 13's here, as are the glibc and the loader
# that run it: the same glibc tools/common/fetch/fetch-rustc-sysroot.sh pins. That is
# the fast way to a browser on Ferrix; ferrousli standing in for that glibc
# is the other, and the same volume serves both.
#
# The volume holds a Debian-shaped tree at its root -- `usr/lib64`,
# `usr/lib/x86_64-linux-gnu` -- and Chrome under `chrome/`. Ferrix mounts it
# at `/data`, and the test's image links the directories glibc names into
# it, as test-rustc's does.
#
# Every download is pinned by its SHA-256: Debian's Packages files for
# trixie and trixie-security on 2026-09-24, and Chrome for Testing's zip as
# downloaded on 2026-09-24 (its own index publishes no digest). After
# unpacking, every library each ELF file on the volume needs is looked for
# on it, so a dependency Debian has and this list lacks fails here rather
# than on Ferrix.
#
# Writes $FERRIX_CHROME_VOLUME/chrome.img (default
# ~/.local/share/ferrix/chrome-2/chrome.img). Needs curl, sha256sum, dpkg-deb,
# unzip, readelf and mkfs.btrfs; no root.
#
# Usage: tools/common/fetch/fetch-chrome.sh

set -euo pipefail

debian=${DEBIAN_MIRROR:-https://deb.debian.org/debian}
security=${DEBIAN_SECURITY_MIRROR:-https://security.debian.org/debian-security}
chrome_dist=${CHROME_FOR_TESTING:-https://storage.googleapis.com/chrome-for-testing-public}
# The volume's directory is versioned (xtask's chrome::VOLUME names the same):
# chrome-2 added GTK 3, and the first, chrome, is left as it was.
out=${FERRIX_CHROME_VOLUME:-$HOME/.local/share/ferrix/chrome-2}
# Where a pinned Debian file comes from once the mirror has dropped it, as
# trixie-security drops a package's update when the next one comes:
# snapshot.debian.org's archive as it was on the days the pins were taken,
# tried in turn. The SHA-256 pins are checked either way.
snapshot=${DEBIAN_SNAPSHOT:-https://snapshot.debian.org/archive}
snapshot_times=(20260924T235959Z 20260927T235959Z)
# The downloads, kept between runs; FERRIX_CHROME_POOL names another
# directory, an empty one to fetch everything as a new machine does.
pool=${FERRIX_CHROME_POOL:-$out/pool}

CHROME_VERSION=154.0.8037.57
CHROME_SHA256=5a6979d0ab7cf952ea575d35164e7bdce4872b2ced8f8a215c8f8e8eda00ee09
# The same version's full browser, which draws windows: Ozone's Wayland
# backend, with its own libwayland, talks to Ferrix's compositor.
CHROME_WINDOW_SHA256=ceee2972074d441ea7c4ba8bcc0eaab77e7e87680f6653d73d3065851fe10302

# Pool path and SHA-256 of each Debian 13 package from the main archive.
debs=(
    "main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb 967aa62605721081c3eb2a17650611a792aa802d76a6511d1840242623d204c9"
    "main/g/gcc-14/libgcc-s1_14.2.0-19_amd64.deb 3c71917b490d1a17aed43196a2787a256ecf060526cdb20216a74bedc061b150"
    "main/g/gcc-14/libatomic1_14.2.0-19_amd64.deb 212b399aae2f7299203d261a57e49372e09565a9a5ea971905f94a3960366c05"
    "main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_amd64.deb 015be740d6236ad114582dea500c1d907f29e16d6db00566ca32fb68d71ac90d"
    "main/a/alsa-lib/libasound2t64_1.2.14-1+deb13u1_amd64.deb 5495496142d57e5ad6d581ac5456ec3c87f872c109a2bfb815d154b673eebc67"
    # alsa-lib's configuration, without which it opens no device at all:
    # even `hw:0,0` is defined in its alsa.conf (docs/AUDIO.md §3.5).
    "main/a/alsa-lib/libasound2-data_1.2.14-1+deb13u1_all.deb 04688afdff3769c0f685541daed7b2f6f0cb946799ddf1d5847ddf11fe245559"
    # libpulse, which Chrome's sound takes over ALSA once it loads, and what
    # it links: its server is pulsed (docs/AUDIO.md, U2d). Pinned from
    # trixie's Packages file on 2026-09-27.
    "main/p/pulseaudio/libpulse0_17.0+dfsg1-2+b1_amd64.deb 786aca79119cb249562708fd24dcdf817604cfe75bbfcb8cda3464042e17f40e"
    "main/liba/libasyncns/libasyncns0_0.8-6+b5_amd64.deb e7958777b0e3586349c57ac759a95b68f80d28e5bd1c4225705198b39b436115"
    "main/libs/libsndfile/libsndfile1_1.2.2-2+deb13u1_amd64.deb d92263a045554e6f39c6b1c2ee4cca44a3248d9b9339301ff195969bf84b75eb"
    "main/f/flac/libflac14_1.5.0+ds-2_amd64.deb 658b9f77d35638caf75dfaea4f6d5cde196104d5662f0776e23d7cc450bd4cb5"
    "main/libo/libogg/libogg0_1.3.5-3+b2_amd64.deb 911aec914f5bda0788164ff8068b682537378bf8d407b1d6c1b377522f96b83b"
    "main/libv/libvorbis/libvorbis0a_1.3.7-3_amd64.deb 0465f7f602f7b00965e58e7273b5876060ab7946f0452a9f95d979046932cc42"
    "main/libv/libvorbis/libvorbisenc2_1.3.7-3_amd64.deb 5fbbf06a5b196235aa2753cd12bbe7f843328571d368f15b1c337578e29f73a2"
    "main/o/opus/libopus0_1.5.2-2_amd64.deb 794056db33d71b2ac4bd8b5a4eb23b627bcb8a49d123c33b6e841a996253a067"
    "main/m/mpg123/libmpg123-0t64_1.32.10-1+deb13u1_amd64.deb 31fa6ef638f1fdbe198faee0b6fe50d9dd468077f8247cc3dbf6b2250abb6101"
    "main/l/lame/libmp3lame0_3.100-6+b3_amd64.deb 3867319c9fbbc8cc25f2d1dc1d75d13548c84c37fa709c1ac9ca02ac051b63ab"
    "main/libx/libx11/libx11-xcb1_1.8.12-1_amd64.deb e05f94d21a932fba5b09b9b13d99df776b155d3bc792c0e294451df9ffe1ba25"
    "main/a/at-spi2-core/libatk-bridge2.0-0t64_2.56.2-1+deb13u2_amd64.deb c0fe87ea1bdca2f938eaa8286af5ad38ecd15cf18c6cfb5961d7d46ac3192325"
    "main/a/at-spi2-core/libatk1.0-0t64_2.56.2-1+deb13u2_amd64.deb 24786bc90e3ff80c4d1d8bdb5ec18bb5f06df9081a5233eb080f3202fcf6a1c3"
    "main/a/at-spi2-core/libatspi2.0-0t64_2.56.2-1+deb13u2_amd64.deb 65e9c9ecbd820fba2d04104cddd0bad33d256cbdfe0d1cb5365f10e363df2315"
    "main/d/dbus/libdbus-1-3_1.16.2-2_amd64.deb bbcb711daff7e104b5f80f9b05475b6142674bfe65518a36a56e96a91068a3f5"
    "main/libd/libdrm/libdrm2_2.4.124-2_amd64.deb fe2276901c7cd7b8079de63072d37fe1cbeb4eb001a3bc1f1d662ad89aa0890e"
    "main/libf/libffi/libffi8_3.4.8-2_amd64.deb 0ebdc340de33333639c3c63874cd4b15ac2e83dfa1ef3053b7eefaf4919f4f68"
    "main/m/mesa/libgbm1_25.0.7-2+deb13u1_amd64.deb 31fb6d76b9ceaf13848fa617df53f85f62626b4fe7464a93811c720af6d5f2dd"
    "main/g/glib2.0/libglib2.0-0t64_2.84.4-3~deb13u5_amd64.deb e2baf92c57d1db1753e5781a61450d9a01e7833c7bd2dfe87776a6811c11ecc2"
    "main/n/nspr/libnspr4_4.36-1_amd64.deb 87fc2039ee89cff2b8010fa9535706b1def40031acfc52bf12197dcb3cb7b064"
    "main/p/pcre2/libpcre2-8-0_10.46-1~deb13u2_amd64.deb 1252b96a5bc44bb5db982bef8eb18e54f5047cede2aff641bce4f8e1edb91c3e"
    "main/libs/libselinux/libselinux1_3.8.1-1_amd64.deb 68bb8d32bd8d6d7d2f5952a169db03d1484b46ae1e52abccdec42a19dccea5d5"
    "main/s/systemd/libsystemd0_257.13-1~deb13u1_amd64.deb ab0d4127b5e46e6f8c015a1db15a62ba9ae274cdefa150083adf90ada0600ea1"
    "main/s/systemd/libudev1_257.13-1~deb13u1_amd64.deb 5d41c284f5a93b05bc7d648b61a02dd2bb9ff05b2261ad1a8b7d96044a0cfa88"
    "main/w/wayland/libwayland-server0_1.23.1-3_amd64.deb 2967212bd582e0dffca443fdc44f4c660e7368d41f7ee3a7f6314e0c3abfe9ea"
    "main/libx/libx11/libx11-6_1.8.12-1_amd64.deb b5a3fd3bf8c8fd0364bfb9bea00dcba7fc301229bd02dded084632d31f5b0fb3"
    "main/libx/libxau/libxau6_1.0.11-1_amd64.deb 689a9f0e0ba3e2c65431f864871e303ee904de69dd28abfc462663fae030227f"
    "main/libx/libxcb/libxcb1_1.17.0-2+b1_amd64.deb 5c222a72d11b866447da31693254f738430726e3e065a384e82687b2fd2f978b"
    "main/libx/libxcomposite/libxcomposite1_0.4.6-1_amd64.deb 20e3c1d9b2135f0c8c4246a9fd26a51a57d5850c3b36ecc173c45d6be7328af3"
    "main/libx/libxdamage/libxdamage1_1.1.6-1+b2_amd64.deb e51e43f23f3befbc1f9408271f4df6773d37caece9ce7e1f38abade382f7fbf7"
    "main/libx/libxdmcp/libxdmcp6_1.1.5-1_amd64.deb 0740dc760916b2008b45417a42a8fd7dd5de370fb57d31373f15034cda8acf0b"
    "main/libx/libxext/libxext6_1.3.4-1+b3_amd64.deb fc618ec40465e5ce48622606299cb47833efc3fb235ba15543b81f850722f443"
    "main/libx/libxfixes/libxfixes3_6.0.0-2+b4_amd64.deb 3fdd95d86d8e9d63e11f52070935c8f9f912c36aa3b17338d969f7685f3ed4fb"
    "main/libx/libxi/libxi6_1.8.2-1_amd64.deb 093d0903f35bb7a9f6815180ee040e6951fecf9b66c128cd72f064710210606e"
    "main/libx/libxkbcommon/libxkbcommon0_1.7.0-2_amd64.deb f75ee544f55acc6a271debfab3ea4ae0458afc89d81cfe1a71137e07d4895b86"
    "main/libx/libxrandr/libxrandr2_1.5.4-1+b3_amd64.deb 11e3490de93a8bbee3daba719cb8e1325a26fb3c125525c34bdcb7deb05eb9b2"
    "main/libx/libxrender/libxrender1_0.9.12-1_amd64.deb 9d042dfd5e613be1e02e6ddd0c5c4adef19c5eb08f6db838c2eba672c496dca4"
    "main/libx/libxres/libxres1_1.2.1-1+b2_amd64.deb 995b467fa9d0d47b4bff46cafd11ac7067f36a0e30cfaa121e73df7f8af93041"
    "main/libc/libcap2/libcap2_2.75-10+deb13u1+b3_amd64.deb 89fc4d34fc7a28ad6f0fcd0c561ab253b9dedf6f77f5a000b47c276c8295bf67"
    "main/s/sqlite3/libsqlite3-0_3.46.1-7+deb13u2_amd64.deb 0a459adaffd901109f7811ab65f58e7a957b4907d05539cf3d1184efdcde0468"
    # fontconfig's configuration and a font: Chrome finds fonts through the
    # fontconfig it carries, and draws no text without one.
    "main/f/fontconfig/fontconfig-config_2.15.0-2.3_amd64.deb 0475c00d02660c07a15085051818625331bb502e053242106aaaa1f2ddb41225"
    "main/f/fonts-dejavu/fonts-dejavu-core_2.37-8_all.deb 86635b3d25b3655fc11cb3ecc3af59f0bf19643b02b94f2de48bd10253cdba12"
    "main/f/fonts-dejavu/fonts-dejavu-mono_2.37-8_all.deb 3003e98a5debfdeadc7040a7f715fe9fe6fb67f68deacf6049b54e30f07fc014"
    "main/c/cairo/libcairo2_1.18.4-1+b1_amd64.deb 01390b049661e4cc8bd39cef78b41c2587d4035619d3b00d6c7846f43d80bbba"
    "main/p/pango1.0/libpango-1.0-0_1.56.3-1_amd64.deb 4fa38472696d2ddb10bfc9a4692a9446e8478fdb1f48c769a609fc9fec3adfc5"
    "main/a/avahi/libavahi-client3_0.8-16_amd64.deb 27df3f1c0190d13ec6d014c5515c2ec8e9a49d0983f48fc9e5756ee4e62d7e37"
    "main/a/avahi/libavahi-common3_0.8-16_amd64.deb fb092fc55f1c7f3db0ac64467f30ebae8145543570b8e99f17ce84cf1dd28bb6"
    "main/f/fontconfig/libfontconfig1_2.15.0-2.3_amd64.deb 7ae91ec59857cc8e375eb2bdd371a4a69daea38f7787d06d474a847867d852b1"
    "main/f/fribidi/libfribidi0_1.0.16-1_amd64.deb f8c35ec8606d0843bd3cf020c6cfa39ddfe717ec58ed826b5855ba9927eb0d55"
    "main/h/harfbuzz/libharfbuzz0b_10.2.0-1+deb13u1_amd64.deb 932165b933e5eb633293afe984a983ebe89cb27c923ce7208626f1da6c0c7941"
    "main/p/pixman/libpixman-1-0_0.44.0-3_amd64.deb 3862c87596f6b4b60fc916395fd69aa10af28a875a8e5c081a8cf30ddd120bd6"
    "main/libt/libthai/libthai0_0.1.29-2+b1_amd64.deb e6364c07aeeffe9a95f267061d9df168e48e9dc9126c3b40e19dc829f67702a7"
    "main/libx/libxcb/libxcb-render0_1.17.0-2+b1_amd64.deb 7f6e36978e0f70a590fbad4042a692c70b50ca7cbcdc53fa7e9d9ab87933acc6"
    "main/libx/libxcb/libxcb-shm0_1.17.0-2+b1_amd64.deb d1777a813f484e89f3dcaadd1d5f2b1a13e9313b6bf70eded16d8ce743208fef"
    "main/b/brotli/libbrotli1_1.1.0-2+b7_amd64.deb 0fb79f88db210afbd69282ab9649e525f393ec6950ca34da1a6b359250b8d7db"
    "main/b/bzip2/libbz2-1.0_1.0.8-6_amd64.deb cba4cda04244b5e481bb15524bc3c983a7d1b6f330013b9b381706a2fcb65310"
    "main/e/e2fsprogs/libcom-err2_1.47.2-3+b12_amd64.deb bd43e020b8fed399db3eebf098111f10fba4c436fff99ccad72cc7e27c9764ec"
    "main/libd/libdatrie/libdatrie1_0.2.13-3+b1_amd64.deb 339eb88f23d72b8f0b7b1aebcefbab6de600d97b88bc6222dd8aad11d2ca9051"
    "main/g/gmp/libgmp10_6.3.0+dfsg-3_amd64.deb d0d0265eb01770f17afd0f7c8c0622f80479dcfbbe13653a0debeec61464e622"
    "main/g/graphite2/libgraphite2-3_1.3.14-2+deb13u1_amd64.deb 1769ba3ed3c1fc75b7fbe02678e32aa24391205813eceef4e1e3500015d12ee3"
    "main/n/nettle/libhogweed6t64_3.10.1-1_amd64.deb b059ed155115ac09da295322de48ee1ed58ef5fa45edcc9e12ff0e94636ef25f"
    "main/libi/libidn2/libidn2-0_2.3.8-2_amd64.deb 90b039bcdc4578f8e1c4935adf8dbb525e36a164deefdbbb8c45bac347d48278"
    "main/n/nettle/libnettle8t64_3.10.1-1_amd64.deb 1b03d4a9cd9c8143ba50fe6396f36937e901a317d492d179f4596862c1731cfe"
    "main/p/p11-kit/libp11-kit0_0.25.5-3_amd64.deb 784bf2063e166c8bc851a32623b74ebd85c499043a9d57c5bbd64fa63447f45a"
    "main/libt/libtasn1-6/libtasn1-6_4.20.0-2+deb13u1_amd64.deb 23fec6e06583ce2bad9b2c04c9b485e90440e259b1abf8677cd80d3ce60831ad"
    "main/libu/libunistring/libunistring5_1.3-2_amd64.deb 6dd3490bef06ea1096f32d10766b7e016cf579bfae8451d1b7df15ce05b8aa46"
    "main/k/keyutils/libkeyutils1_1.6.3-6_amd64.deb 0b11ad17be0300b63ad4eeb4c6450fed24d34b7b740f23e5363dcb29ee6d5eba"
    # GTK 3, which Chrome loads for its system look, its dark mode and its
    # file chooser (Yoru's Nocturne, docs/DESKTOP.md §6), and what it loads.
    # The keyboard's layouts, which GTK's xkbcommon reads and aborts without.
    "main/x/xkeyboard-config/xkb-data_2.42-1_all.deb 196ff18533382f64e057ea49df2bb486bd4275a4cc0917361edb560b8756dada"
    "main/a/adwaita-icon-theme/adwaita-icon-theme_48.1-1_all.deb f60b0577aee314d43d6908ca7f0b8f07dc48e2bb766e7e25035330817e0e827c"
    "main/h/hicolor-icon-theme/hicolor-icon-theme_0.18-2_all.deb d91fef83a186d3fc9fff293bfd7da4f684f736d15459d258a5a0d251168c1a35"
    "main/c/cairo/libcairo-gobject2_1.18.4-1+b1_amd64.deb 4fcd432ebe8f93599bd05426f304e2f70409aad271fc6b4b3a2c6243c99c377c"
    "main/libc/libcloudproviders/libcloudproviders0_0.3.6-2_amd64.deb a626bb23ed9ebc4bc3ce446b552048de63d76768bf0fb492754965c5ee28a7e4"
    "main/c/colord/libcolord2_1.4.7-3_amd64.deb f11fd3b492781122edf0790f1e1fe24c51a41c67d4bb76cff84a9426c5654fa7"
    "main/libd/libdeflate/libdeflate0_1.23-2_amd64.deb 5fe37fb79d53f5c91da6ba3ac4c1a8453e1f2b1ff0e0ea45189c5338bbfdbf82"
    "main/libe/libepoxy/libepoxy0_1.5.10-2_amd64.deb 4c4c8024f2175086de65bca9fdc3fbb967f2863ebf249d489c66d0c8103dd3a3"
    "main/g/gtk+3.0/libgtk-3-0t64_3.24.49-3_amd64.deb 4aa8828f01365f937f3bf69f812c8107ee4c8a2b208ffdd7656e8de5de81d98f"
    "main/g/gtk+3.0/libgtk-3-common_3.24.49-3_all.deb 9161bd6b9933384073ac5bacd6a3bb1e2998877a605411d73041fb0b7fadd605"
    "main/j/jbigkit/libjbig0_2.1-6.1+b2_amd64.deb 95cbc44561651e1b3385454df08e8f2efe84498f6963a85259be2cc2fe798709"
    "main/libj/libjpeg-turbo/libjpeg62-turbo_2.1.5-4_amd64.deb 5504299ad0e3c0a9ca9f962a68a650a040db52f9f416a109e74df48d55601d3f"
    "main/l/lerc/liblerc4_4.0.0+ds-5_amd64.deb 2e0c13c0ddbde12968c2c713c2e0474a5b78e90d5216031256a725e8c76aa31c"
    "main/x/xz-utils/liblzma5_5.8.1-1+deb13u1_amd64.deb 1cfcc6e0dc36f438a79b6e2189facdb9d150b08f57d190a60e01c98075c7f896"
    "main/p/pango1.0/libpangocairo-1.0-0_1.56.3-1_amd64.deb 6994729c5b7a9649661ec078cf039991f22eff44600543aa917894f83c89cd05"
    "main/p/pango1.0/libpangoft2-1.0-0_1.56.3-1_amd64.deb b506cf66f78ddf542ddb4bb4a68b3d748d41caa6e190e4480430a6630c1e5105"
    "main/libw/libwebp/libsharpyuv0_1.5.0-0.1_amd64.deb 51934833d909588f7761fd84362114e90d8d2d38af57edf5df5c4a705e037547"
    "main/g/gcc-14/libstdc++6_14.2.0-19_amd64.deb ab1fa05837aa7a92aae748fd07a18a35f7d18bb4a71c4724fe2bbf0e32089de0"
    "main/w/wayland/libwayland-client0_1.23.1-3_amd64.deb d1607e1db1a7c5378a2e5c6ae7b20f64dc6acf134f953d84a7f5204b74339f33"
    "main/w/wayland/libwayland-cursor0_1.23.1-3_amd64.deb 26fccae2b1228744de0cad6c688550b8d13cd4e8d4ccf0cfdae015634ece673f"
    "main/w/wayland/libwayland-egl1_1.23.1-3_amd64.deb 7c5c3429028b39b9432792db08cb218c43891df439ebc648b83ebee676a80ec5"
    "main/libw/libwebp/libwebp7_1.5.0-0.1_amd64.deb c749744b04abdd1426b0c7718e516362c03a557c5151662092d6d4f2cef6e7c1"
    "main/libx/libxcursor/libxcursor1_1.2.3-1_amd64.deb bb0dbf58b804faaa74ed9efdae61c4b14bb0b4855680798f63e5ca6a4954976b"
    "main/libx/libxinerama/libxinerama1_1.1.4-3+b4_amd64.deb 49d4e1628960407b8be2880a42428e7629801715a8a5697d42c0b0ca0a479911"
    "main/libz/libzstd/libzstd1_1.5.7+dfsg-1_amd64.deb 2f6a2aeacfc925eba8b00ac9139bc4bfccf8cacb09eb93de067074b26948eef9"
)

# The same, from the security archive.
security_debs=(
    "updates/main/u/util-linux/libblkid1_2.41.5-0+deb13u1_amd64.deb 81535f3c2c0efc732965907c8749103a0a26377c761622c9ce39b4c92dcde52f"
    "updates/main/u/util-linux/libmount1_2.41.5-0+deb13u1_amd64.deb 6d00f45f2e80e078e906e3eedecd3ba6913e39fef49bff361ee583f17f00ec05"
    "updates/main/e/expat/libexpat1_2.8.3-1~deb13u1_amd64.deb 38abe0e710a07688e9c149d74536e67cfee0364bdb64dd6d644c32a1cfad389f"
    "updates/main/n/nss/libnss3_3.110-1+deb13u4_amd64.deb 8d20d0754039e15e9bd2c8484e98dc9c2982907d4f20834da29a9501913d00b5"
    "updates/main/c/cups/libcups2t64_2.4.10-3+deb13u1_amd64.deb 3ad4c8a3aeb00f3e5c950e43e92618f1b6ebffc6a1d7d46a62c75a56098dded4"
    "updates/main/f/freetype/libfreetype6_2.13.3+dfsg-1+deb13u1_amd64.deb e4947f3291528f03d574f2b01d5c5fc45c58c47480f32888a5cdf0f231ab584e"
    "updates/main/g/gnutls28/libgnutls30t64_3.8.9-3+deb13u4_amd64.deb 18a8bdfd91c7e3bcb01719d55a2b56849c7160b34f1f52b1c4fdfdd41bf1352b"
    "updates/main/k/krb5/libgssapi-krb5-2_1.21.3-5+deb13u1_amd64.deb 30847c1fde4240567d7ed3aeab4f655dd591203758b857e85e824045aae70299"
    "updates/main/libp/libpng1.6/libpng16-16t64_1.6.48-1+deb13u5_amd64.deb 2465b4e9fa85cff54dc10a6da3e64074d8d9292c86e4a8f989b809d6b158e97e"
    "updates/main/k/krb5/libk5crypto3_1.21.3-5+deb13u1_amd64.deb 7da07ee674b47f1f0be7cc89317c25310086a1f1761217d0f72e6ae2c5a69b84"
    "updates/main/k/krb5/libkrb5-3_1.21.3-5+deb13u1_amd64.deb 47d71d6a7f2e59b9bae5f89602397594805113b95889ad18fa703cd53abafc97"
    "updates/main/k/krb5/libkrb5support0_1.21.3-5+deb13u1_amd64.deb 3a0acd8b37955c0e102c756b52c97df2a31f67b96453c35dab70df218d309117"
    # GTK 3's, from the security archive.
    "updates/main/g/gdk-pixbuf/libgdk-pixbuf-2.0-0_2.42.12+dfsg-4+deb13u1_amd64.deb 94150fb955b2d3a9a5e054e31c5a4b048bad4a479207e86cde46fbc975c838b4"
    "updates/main/g/gdk-pixbuf/libgdk-pixbuf2.0-common_2.42.12+dfsg-4+deb13u1_all.deb 9f0c64e33cf5bb3226a5a6ddf630dd76530596e476e6b47a8b27da444ff43a03"
    "updates/main/l/lcms2/liblcms2-2_2.16-2+deb13u2_amd64.deb 32e53c2002ccea8c60c02779bea0ba49ac132fef702988a30d64af79b6617806"
    "updates/main/t/tiff/libtiff6_4.7.0-3+deb13u3_amd64.deb 6bb2038f36817ae1ee6d33df03917b21835dcc2e0f3e3e80a71390e83c171173"
)

for tool in curl sha256sum dpkg-deb unzip readelf mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-chrome: $tool is not installed" >&2; exit 1; }
done

# Download $2/$1 from snapshot.debian.org into $3, $2 being $debian's or
# $security's tree, at each of the snapshot times until one has it; fail when
# $2 is not Debian's or no snapshot has the file.
from_snapshot() {
    local path=$1 base=$2 part=$3 archive rel time
    case $base in
        "$security"*) archive=debian-security rel=${base#"$security"} ;;
        "$debian"*) archive=debian rel=${base#"$debian"} ;;
        *) echo "fetch-chrome: $base/$path could not be downloaded" >&2; return 1 ;;
    esac
    for time in "${snapshot_times[@]}"; do
        echo "fetch-chrome: ${path##*/} is not at $base; from $snapshot/$archive/$time" >&2
        curl -fsSL --retry 3 -o "$part" "$snapshot/$archive/$time$rel/$path" && return 0
    done
    echo "fetch-chrome: ${path##*/} is in no snapshot either" >&2
    rm -f "$part"
    return 1
}

# Download $2/$1 into the pool unless it is there, from the snapshot when $2
# is Debian's and no longer has it, and check it against $3.
fetch() {
    local path=$1 base=$2 sum=$3
    local file="$pool/${path##*/}"
    if [ ! -f "$file" ]; then
        curl -fsSL -o "$file.part" "$base/$path" \
            || from_snapshot "$path" "$base" "$file.part" || exit 1
        mv "$file.part" "$file"
    fi
    echo "$sum  $file" | sha256sum -c --quiet \
        || { echo "fetch-chrome: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

mkdir -p "$pool"
tree="$out/tree"
rm -rf "$tree"
mkdir -p "$tree"

for entry in "${debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$debian/pool" "$sum")" "$tree"
done
for entry in "${security_debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$security/pool" "$sum")" "$tree"
done

zip=$(fetch "$CHROME_VERSION/linux64/chrome-headless-shell-linux64.zip" "$chrome_dist" "$CHROME_SHA256")
unpacked=$(mktemp -d)
trap 'rm -rf "$unpacked"' EXIT
unzip -q "$zip" -d "$unpacked"
mv "$unpacked/chrome-headless-shell-linux64" "$tree/chrome"
window_zip=$(fetch "$CHROME_VERSION/linux64/chrome-linux64.zip" "$chrome_dist" "$CHROME_WINDOW_SHA256")
unzip -q "$window_zip" -d "$unpacked"
mv "$unpacked/chrome-linux64" "$tree/chrome-window"

# Documentation and manuals, which nothing runs.
rm -rf "$tree/usr/share/doc" "$tree/usr/share/man" "$tree/usr/share/lintian"

test -x "$tree/chrome/chrome-headless-shell" \
    || { echo "fetch-chrome: no chrome-headless-shell in the tree" >&2; exit 1; }
test -x "$tree/chrome-window/chrome" \
    || { echo "fetch-chrome: no chrome in the tree" >&2; exit 1; }
test -e "$tree/usr/lib64/ld-linux-x86-64.so.2" \
    || { echo "fetch-chrome: no linker at usr/lib64" >&2; exit 1; }

# Every library an ELF file on the volume needs must be on it: in glibc's
# directory, beside Chrome, or glibc itself.
missing=0
while IFS= read -r -d '' file; do
    head -c 4 "$file" | grep -q $'\x7fELF' || continue
    # The directories the file names itself, as libpulse names its private
    # pulseaudio/ one, which the loader searches for it alone.
    runpath=$(readelf -d "$file" 2> /dev/null | sed -n 's/.*(\(RUNPATH\|RPATH\)).*\[\(.*\)\]/\2/p' | tr ':' ' ')
    for needed in $(readelf -d "$file" 2> /dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
        own=0
        for dir in $runpath; do
            [ -e "$tree$dir/$needed" ] && own=1
        done
        if [ "$own" = 0 ] && [ ! -e "$tree/usr/lib/x86_64-linux-gnu/$needed" ] \
            && [ ! -e "$tree/lib/x86_64-linux-gnu/$needed" ] \
            && [ ! -e "$tree/chrome/$needed" ] \
            && [ ! -e "$tree/chrome-window/$needed" ] \
            && [ ! -e "$tree/usr/lib64/$needed" ]; then
            echo "fetch-chrome: ${file#"$tree"/} needs $needed, which is not on the volume" >&2
            missing=1
        fi
    done
done < <(find "$tree/chrome" "$tree/chrome-window" "$tree/usr/lib/x86_64-linux-gnu" \
    "$tree/usr/lib/x86_64-linux-gnu/pulseaudio" -maxdepth 1 -type f -print0)
[ "$missing" = 0 ] || exit 1

# Room for Chrome's profile, caches and the screenshot, and a size that does
# not depend on how mkfs rounds.
size=$(( $(du -sm "$tree" | cut -f1) + 512 ))
image="$out/chrome.img"
rm -f "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
# The tree stays beside the image, so the same files can be tried on the host.
echo "chrome volume: $image (${size} MiB), unpacked in $tree"
