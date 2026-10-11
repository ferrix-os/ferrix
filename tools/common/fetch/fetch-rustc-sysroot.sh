#!/usr/bin/env bash
# Fetch the upstream Rust compiler, Cargo and the Debian userland they need,
# and put them on a btrfs volume: the disk stage 16's exit compiles on
# (docs/ROADMAP.md, `cargo xtask test-rustc`), and the toolchain stage 20's
# `cargo xtask test-selfhost` builds Ferrix with.
#
# Nothing here is built. rustc and Cargo are the rust-lang.org releases, both
# dynamically linked against glibc; rustc's LLVM is a shared library of its
# own. rustc links through `cc`, which is gcc 15's driver, and gcc runs
# `collect2`, which runs the `ld.lld` rustc points it at, which runs
# `rust-lld`. Every one of those is somebody else's glibc binary, and the
# glibc is Debian 13's, the same one tools/common/fetch/fetch-debian-busybox.sh pins.
# Beside the host's standard library are the ones Ferrix's own images are
# built against: the kernels' and native programs' freestanding targets, the
# loaders' UEFI targets (and the ARMv7-A loader's musl one, used for its
# position-independent `core`), and the two musl targets zinc is built for.
#
# The volume holds a Debian-shaped tree at its root -- `usr/bin`,
# `usr/lib/x86_64-linux-gnu`, `usr/lib64`, `usr/lib/gcc`, `usr/libexec` --
# and the toolchain under `rust/`. Ferrix mounts it at `/data`, and its images
# link the directories glibc's and gcc's own paths name into it.
#
# Every download is pinned by the SHA-256 its own index gave: Debian's
# Packages files for trixie and sid at snapshot.debian.org's 20260923T000000Z,
# which is also where they are fetched from, since the mirrors drop a package
# a point release replaces; and the channel manifest channel-rust-1.97.1.toml
# (the `xz_hash` of each component).
# With them come rustfmt, clippy, rust-analyzer and the standard library's
# source, of the same release: what `cargo xtask check` and an editor run
# when the tree is worked on from within Ferrix (docs/DEV-ON-FERRIX.md).
# And an editor that speaks to that language server: helix's own release
# for the host's architecture, its program and its runtime (grammars,
# queries, themes) under `helix/`, where the program finds the runtime
# beside itself; images link `/bin/hx` to it. It is pinned by the SHA-256
# of the release's tarball as downloaded on 2026-10-11, since the project
# publishes none.
# Beside the compiler is what stage 20's builds of Ferrix's C programs run:
# busybox, uutils and the ports against ferrousli are compiled by gcc and
# g++ 15 (`cc1` and `cc1plus`) and assembled and archived by binutils,
# driven by make, cmake, ninja, bash and dash, over GNU coreutils, sed, grep,
# gawk, findutils, diffutils, tar, the compressors and file, with the
# kernel's UAPI headers from linux-libc-dev, and Perl and Python for the
# generators mbedTLS's test server is built with; foot's libraries add
# meson, bison, pkg-config and wayland-scanner.
# Those packages, and the libraries they need, were resolved from the same
# Packages file by their Depends; nothing that runs only at installation
# (debconf, dpkg, PAM) is taken.
#
# Writes $FERRIX_RUSTC_SYSROOT/rustc.img (default
# ~/.local/share/ferrix/rustc/rustc.img, or ~/.local/share/ferrix/rustc-arm64/
# with FERRIX_SYSROOT_ARCH=arm64, which takes the same packages for a Ferrix
# that compiles on AArch64 from rustc-sysroot-arm64.sh beside this script). Needs curl, sha256sum, dpkg-deb, tar
# and mkfs.btrfs; no root, since mkfs.btrfs --rootdir writes an image file.
#
# Usage: tools/common/fetch/fetch-rustc-sysroot.sh

set -euo pipefail

debian=${DEBIAN_MIRROR:-https://snapshot.debian.org/archive/debian/20260923T000000Z}
helix_releases=${HELIX_RELEASES:-https://github.com/helix-editor/helix/releases/download}
rust=${RUST_DIST_SERVER:-https://static.rust-lang.org}
arch=${FERRIX_SYSROOT_ARCH:-amd64}
case "$arch" in
    amd64) gnu=x86_64-linux-gnu triple=x86_64-unknown-linux-gnu loader=usr/lib64/ld-linux-x86-64.so.2
        helix="25.07.1/helix-25.07.1-x86_64-linux.tar.xz 3f08e63ecd388fff657ad39722f88bb03dcf326f1f2da2700d99e1dc40ab2e8b"
        out=${FERRIX_RUSTC_SYSROOT:-$HOME/.local/share/ferrix/rustc} ;;
    arm64) gnu=aarch64-linux-gnu triple=aarch64-unknown-linux-gnu loader=usr/lib/ld-linux-aarch64.so.1
        helix="25.07.1/helix-25.07.1-aarch64-linux.tar.xz ce23fa8d395e633e3e54c052012f11965d91d8d5c2bfa659685f50430b4f8175"
        out=${FERRIX_RUSTC_SYSROOT:-$HOME/.local/share/ferrix/rustc-arm64} ;;
    *) echo "fetch-rustc-sysroot: FERRIX_SYSROOT_ARCH is amd64 or arm64, not $arch" >&2; exit 1 ;;
esac

# Pool path and SHA-256 of each Debian 13 (trixie) package.
debs=(
    "main/b/bash/bash_5.2.37-2+b10_amd64.deb 2fd7b04f1b7caa29c4e683f5216e6af354a41e3cdcf6e75394d7cda680f9ab82"
    "main/b/binutils/binutils_2.44-3_amd64.deb 6bc08c02539ba53b5e748142397144c499f9b20b5fa9bb56431545db124addeb"
    "main/b/binutils/binutils-common_2.44-3_amd64.deb 002da5d23f8757dee97a2c0a40e0e1d4d85a43da094488ee2ee7068d4d3691f9"
    "main/b/binutils/binutils-x86-64-linux-gnu_2.44-3_amd64.deb e6741ce95ff0f7a131c8d9faa3528ccbbc453078bbc62a97da81340ed7462c53"
    "main/b/bzip2/bzip2_1.0.8-6_amd64.deb 6b5fbe71f0fc41e85919bc4b3c514e6c47577c5005dbf1f5af6e4a49cc221aab"
    "main/c/cmake/cmake_3.31.6-2_amd64.deb dee2fc94973325240604ffbca677a3f16b462fb6520946bd81be1ecbac770a02"
    "main/c/cmake/cmake-data_3.31.6-2_all.deb d9060145be892f43cd1888024114ceab019c81479d4b0a3383fe14cebe2295cb"
    "main/c/coreutils/coreutils_9.7-3_amd64.deb 1299ab6f9389a288eb2f5f3dd222c26cc777b9a2d5ecb6ee4cbd340cebcdada2"
    "main/d/dash/dash_0.5.12-12_amd64.deb a8902cb6d8650134764a25fb80aec8589d858ef71ece1680a62e84816c37bb04"
    "main/d/diffutils/diffutils_3.10-4_amd64.deb d3412669f7ae53ba895c3a509620ea7494ec1a36faa204c5215442ced381a479"
    "main/f/file/file_5.46-5_amd64.deb 5b980b02a7d91aab9cb50c65ac7e059678a36d36ed626ec52506442f4501a1a7"
    "main/f/findutils/findutils_4.10.0-3_amd64.deb 3edf02b016456b6a98cefdbab99c5ce6f829b786f63affe970badea221aecfc7"
    "main/g/gawk/gawk_5.2.1-2+b1_amd64.deb 0e3b376e74cadfa5049bc8248c563efe89fd45e31466f8891c094c65c0879f5c"
    "main/g/grep/grep_3.11-4_amd64.deb 2d78ed07d86a530ebf538f05cb2415f37c8daf908596bc32fc2563cc7e88c55a"
    "main/g/gzip/gzip_1.13-1+deb13u1_amd64.deb 74cf12212beee4ab8d473bdc0107abf9e8cef737492198e2f4944a19f142b0ac"
    "main/a/acl/libacl1_2.3.2-2+b1_amd64.deb 08074f01e384bc07c0c2d79a58cf4a6523f71cf75d1808101c79617656c9a39d"
    "main/liba/libarchive/libarchive13t64_3.7.4-4+deb13u1_amd64.deb 12e06195a899e8db371803ed70111fc7302573d3ed84e967d3d48ef2d543ace8"
    "main/a/attr/libattr1_2.5.2-3_amd64.deb 606b5ee12ea2786be607a17f40c1fb5e65c76ceaff66665bdf8f8c6c1b71d1fb"
    "main/b/binutils/libbinutils_2.44-3_amd64.deb 4f4664c8a8f0ad0c8631c39fab02e3d8d86ccc6f4436a1d59f059dbcb0492679"
    "main/b/brotli/libbrotli1_1.1.0-2+b7_amd64.deb 0fb79f88db210afbd69282ab9649e525f393ec6950ca34da1a6b359250b8d7db"
    "main/b/bzip2/libbz2-1.0_1.0.8-6_amd64.deb cba4cda04244b5e481bb15524bc3c983a7d1b6f330013b9b381706a2fcb65310"
    "main/g/glibc/libc-dev-bin_2.41-12+deb13u4_amd64.deb 2c21175d6a8283ed566d154c0c0bc06f60836e53b0b9e1f74c14eac586b68c24"
    "main/g/glibc/libc6_2.41-12+deb13u4_amd64.deb 967aa62605721081c3eb2a17650611a792aa802d76a6511d1840242623d204c9"
    "main/g/glibc/libc6-dev_2.41-12+deb13u4_amd64.deb 1fda734dabcd80b77266a09ab62b0f1e3e16d8091db62899890b2200745632a2"
    "main/libc/libcap2/libcap2_2.75-10+deb13u1+b3_amd64.deb 89fc4d34fc7a28ad6f0fcd0c561ab253b9dedf6f77f5a000b47c276c8295bf67"
    "main/e/e2fsprogs/libcom-err2_1.47.2-3+b12_amd64.deb bd43e020b8fed399db3eebf098111f10fba4c436fff99ccad72cc7e27c9764ec"
    "main/libx/libxcrypt/libcrypt-dev_4.4.38-1_amd64.deb 98e2333aea8d64ca68f9b75c16256d3a05492fd8f9e6dba96adbc6f19e4f5a09"
    "main/libx/libxcrypt/libcrypt1_4.4.38-1_amd64.deb 0ebc144d662e3197982d1bf3a7b8b35ca845e54c68811de0328b1f0d7c67585c"
    "main/b/binutils/libctf-nobfd0_2.44-3_amd64.deb e280b2be3db6e584500e865c251605b95c346767d87ccb2524e44992048fc657"
    "main/b/binutils/libctf0_2.44-3_amd64.deb 120cafcd93132a276fa92a8fb4cf39b23d14e5a3e348f4f5580638d71ca95ac5"
    "main/c/curl/libcurl4t64_8.14.1-2+deb13u5_amd64.deb 0dedddfe614fc4a4335c9167b73c64a50136edd76b7991aa851408d0795163eb"
    "main/d/db5.3/libdb5.3t64_5.3.28+dfsg2-9_amd64.deb 18d02510ee78b67e4504ba050176797a200ff24214a7cd318082ab60ad7bf3fc"
    "main/e/expat/libexpat1_2.8.3-1~deb13u1_amd64.deb 38abe0e710a07688e9c149d74536e67cfee0364bdb64dd6d644c32a1cfad389f"
    "main/libf/libffi/libffi8_3.4.8-2_amd64.deb 0ebdc340de33333639c3c63874cd4b15ac2e83dfa1ef3053b7eefaf4919f4f68"
    "main/g/gdbm/libgdbm-compat4t64_1.24-2_amd64.deb 2cbd43cf2dfbf57ff48188b5d79d29cf7ea8f0dedaa61ab0bcc02eceff50ea01"
    "main/g/gdbm/libgdbm6t64_1.24-2_amd64.deb e7b42c68c391e278733adb3c1efdacd24d660862bd7bac0efbc10a91e5696dfc"
    "main/g/gmp/libgmp10_6.3.0+dfsg-3_amd64.deb d0d0265eb01770f17afd0f7c8c0622f80479dcfbbe13653a0debeec61464e622"
    "main/g/gnutls28/libgnutls30t64_3.8.9-3+deb13u4_amd64.deb 18a8bdfd91c7e3bcb01719d55a2b56849c7160b34f1f52b1c4fdfdd41bf1352b"
    "main/b/binutils/libgprofng0_2.44-3_amd64.deb 0d36d0bde85c467ac6735dded08e19dd30ae1369e3908b9eb48ccc0d6ff66648"
    "main/k/krb5/libgssapi-krb5-2_1.21.3-5+deb13u1_amd64.deb 30847c1fde4240567d7ed3aeab4f655dd591203758b857e85e824045aae70299"
    "main/n/nettle/libhogweed6t64_3.10.1-1_amd64.deb b059ed155115ac09da295322de48ee1ed58ef5fa45edcc9e12ff0e94636ef25f"
    "main/libi/libidn2/libidn2-0_2.3.8-2_amd64.deb 90b039bcdc4578f8e1c4935adf8dbb525e36a164deefdbbb8c45bac347d48278"
    "main/j/jansson/libjansson4_2.14-2+b3_amd64.deb 60707a62fe6c1228c3389b12a13ca4efd76defc5532473e547a29e99cf7d2a6e"
    "main/libj/libjsoncpp/libjsoncpp26_1.9.6-3_amd64.deb fd0b75839fe7b1a08df49e4203b1fb042df4f7d982db8d91c1fd7bdbcd4b1460"
    "main/k/krb5/libk5crypto3_1.21.3-5+deb13u1_amd64.deb 7da07ee674b47f1f0be7cc89317c25310086a1f1761217d0f72e6ae2c5a69b84"
    "main/k/keyutils/libkeyutils1_1.6.3-6_amd64.deb 0b11ad17be0300b63ad4eeb4c6450fed24d34b7b740f23e5363dcb29ee6d5eba"
    "main/k/krb5/libkrb5-3_1.21.3-5+deb13u1_amd64.deb 47d71d6a7f2e59b9bae5f89602397594805113b95889ad18fa703cd53abafc97"
    "main/k/krb5/libkrb5support0_1.21.3-5+deb13u1_amd64.deb 3a0acd8b37955c0e102c756b52c97df2a31f67b96453c35dab70df218d309117"
    "main/o/openldap/libldap2_2.6.10+dfsg-1_amd64.deb 60069e4a550ca890113fa89d3001417781b18e146ec5d637d31e368dec9909bd"
    "main/l/lz4/liblz4-1_1.10.0-4_amd64.deb c31ec4c7c82755a38b2f3fe066fc0c5518cc91a601a268fbcd19bdacb1f22e1e"
    "main/x/xz-utils/liblzma5_5.8.1-1+deb13u1_amd64.deb 1cfcc6e0dc36f438a79b6e2189facdb9d150b08f57d190a60e01c98075c7f896"
    "main/f/file/libmagic-mgc_5.46-5_amd64.deb 1368da7c4c7dd10fb2f9ee3ed8a70801650c0c1d249a4076af15e2ddfbb14b46"
    "main/f/file/libmagic1t64_5.46-5_amd64.deb 53f653873216b135570b42c423ea93825c008ac464780b4578e80ee424237a4e"
    "main/m/mpfr4/libmpfr6_4.2.2-1_amd64.deb 75dddce11dabc7fc543712c33dc27b7f2ee66a111763eb5eac654d010b42cd92"
    "main/n/ncurses/libncursesw6_6.5+20250216-2_amd64.deb 47baa2e11579583654583a066b69bd5a5d2f22423081f707ed6c0c3ba6d538b7"
    "main/n/nettle/libnettle8t64_3.10.1-1_amd64.deb 1b03d4a9cd9c8143ba50fe6396f36937e901a317d492d179f4596862c1731cfe"
    "main/n/nghttp2/libnghttp2-14_1.64.0-1.1+deb13u1_amd64.deb 896cb217537c09251fb909b8541349010ac279d802d871e98c06a62a2c67ce2c"
    "main/n/nghttp3/libnghttp3-9_1.8.0-1_amd64.deb 388e4c33b72829cbf0d098afdc8539fa5ba7458eddb077f5538fdb4d5538716b"
    "main/p/p11-kit/libp11-kit0_0.25.5-3_amd64.deb 784bf2063e166c8bc851a32623b74ebd85c499043a9d57c5bbd64fa63447f45a"
    "main/p/pcre2/libpcre2-8-0_10.46-1~deb13u2_amd64.deb 1252b96a5bc44bb5db982bef8eb18e54f5047cede2aff641bce4f8e1edb91c3e"
    "main/p/perl/libperl5.40_5.40.1-6+deb13u1_amd64.deb e44777ff47ad248d1debd57e1f96ab5e89061a341008ae39880ba3b5fc15f273"
    "main/p/procps/libproc2-0_4.0.4-9_amd64.deb c9d61caab1b2ddfc3f17edb95be31b9809fcb2daf27d853dff615bc4ebd08b4b"
    "main/libp/libpsl/libpsl5t64_0.21.2-1.1+b1_amd64.deb 59d42bb1f9ebc0d1776fe616efb08a7a8568b05982e00f70b03c63863db768ab"
    "main/p/python3-defaults/libpython3-stdlib_3.13.5-1_amd64.deb 7e142ed64a81ccdd39714f213467bd365736ef341adb2be11be981f638b9175f"
    "main/p/python3.13/libpython3.13-minimal_3.13.5-2+deb13u5_amd64.deb a4aa6a9a8c77f87bfe545e1d14ec42c001d189ace90e3aa75deec882b39dee49"
    "main/p/python3.13/libpython3.13-stdlib_3.13.5-2+deb13u5_amd64.deb db161322a3481d2c0c3b9a3b9a03c3ab0e2b1718f54b88755fb4a3f939165b84"
    "main/r/readline/libreadline8t64_8.2-6_amd64.deb eeadf2b5e755c9f183883feea2d9b5b28560284275d5f54d3e55d0923b1d0967"
    "main/r/rhash/librhash1_1.4.5-1_amd64.deb 9bf56d7d8ce3e7680a0dd8e3cfe8acdf50e0aa3b98b0a907cf744a4d1711af72"
    "main/r/rtmpdump/librtmp1_2.4+20151223.gitfa8646d.1-2+b5_amd64.deb 93baa2004cbe6c8721b9e81beed078612540aef120970b4751305c51c6697368"
    "main/c/cyrus-sasl2/libsasl2-2_2.1.28+dfsg1-9_amd64.deb 66e49d7ae026811b49a0050f18b0325960a2625ffd1ff60d91b7844a815fa9d2"
    "main/c/cyrus-sasl2/libsasl2-modules-db_2.1.28+dfsg1-9_amd64.deb a5c38659fbd62c33c6d9bfa2be43e75a572d1818531aad32dd74d0a58cc4bd19"
    "main/libs/libselinux/libselinux1_3.8.1-1_amd64.deb 68bb8d32bd8d6d7d2f5952a169db03d1484b46ae1e52abccdec42a19dccea5d5"
    "main/b/binutils/libsframe1_2.44-3_amd64.deb 38f625dfdc582717029ac3a3e97c51d994ec2e7a0e9b230c6b44e40d1276311f"
    "main/libs/libsigsegv/libsigsegv2_2.14-1+b2_amd64.deb bac603b965f303ea88dc39cd723b3d88cd86e3be9c948783f0d5e0d00e971302"
    "main/s/sqlite3/libsqlite3-0_3.46.1-7+deb13u2_amd64.deb 0a459adaffd901109f7811ab65f58e7a957b4907d05539cf3d1184efdcde0468"
    "main/libs/libssh2/libssh2-1t64_1.11.1-1+deb13u2_amd64.deb dbb1024c192d4d292b7cfa902b96076cfe81b56a5eed4fb28da36e8bf543e49a"
    "main/o/openssl/libssl3t64_3.5.7-1~deb13u2_amd64.deb 916f7f40b34a06e6ebfaefcdab331bff458328411da672598f126a760472467d"
    "main/s/systemd/libsystemd0_257.13-1~deb13u1_amd64.deb ab0d4127b5e46e6f8c015a1db15a62ba9ae274cdefa150083adf90ada0600ea1"
    "main/libt/libtasn1-6/libtasn1-6_4.20.0-2+deb13u1_amd64.deb 23fec6e06583ce2bad9b2c04c9b485e90440e259b1abf8677cd80d3ce60831ad"
    "main/n/ncurses/libtinfo6_6.5+20250216-2_amd64.deb 8b9f6a7983e9418564e48a627518de4c03917b56efe68d7f3e93bd8fffa1cc10"
    "main/libu/libunistring/libunistring5_1.3-2_amd64.deb 6dd3490bef06ea1096f32d10766b7e016cf579bfae8451d1b7df15ce05b8aa46"
    "main/u/util-linux/libuuid1_2.41.5-0+deb13u1_amd64.deb c1bf4c4c3ff48c57fabf93307dfb56996b60cfa33927afc4158b5db36fb2721e"
    "main/libu/libuv1/libuv1t64_1.50.0-2_amd64.deb 5fec8035002e0c745b64d1c5b3e1dc42790d1a77574315b976cf3e4aab9ddc0a"
    "main/libx/libxml2/libxml2_2.12.7+dfsg+really2.9.14-2.1+deb13u3_amd64.deb e0c6b63ce4602a036a526f60fe5e6c1586710688058d98fc1001b9b3147b7efd"
    "main/x/xxhash/libxxhash0_0.8.3-2_amd64.deb 81da7064d56fc044f5db4bb3c1e80ff50a4986dcbbf80d958eeca565b44c157f"
    "main/libz/libzstd/libzstd1_1.5.7+dfsg-1_amd64.deb 2f6a2aeacfc925eba8b00ac9139bc4bfccf8cacb09eb93de067074b26948eef9"
    "main/l/linux/linux-libc-dev_6.12.94-1_all.deb 6183985d8fa4b97d277e8b55b10ad247bd98bc23aa8b531c1f79f05bfbf50997"
    "main/m/make-dfsg/make_4.4.1-2_amd64.deb 70a9709e665383b06068ccd423d62ea191b65355f318a25565dbf324b254cec6"
    "main/n/ninja-build/ninja-build_1.12.1-1_amd64.deb bc172ce9270bd6fcd6d5335a83ff9d3c7ba0d13ec624cfdfa1b5aa06901dc453"
    "main/o/openssl/openssl-provider-legacy_3.5.7-1~deb13u2_amd64.deb f155c8191ae6d41da73d792f4182680aeafeb85c3dd223934ee9fdd115c4f1fa"
    "main/p/perl/perl_5.40.1-6+deb13u1_amd64.deb ef145c247787d6f40dcdefd5b417502484827cde0d555998c17fd5b27ec1e1dd"
    "main/p/perl/perl-base_5.40.1-6+deb13u1_amd64.deb b795464137a0f4d443fc9284f4b93e883fb83883cb533adf300ac660807a352a"
    "main/p/perl/perl-modules-5.40_5.40.1-6+deb13u1_all.deb 4c46cdd8f3135aa1c95f07e817f8e5d9be590687441ac1c27e3a8c49400f35ce"
    "main/p/procps/procps_4.0.4-9_amd64.deb 4db90bb6776772fc52585d6b1fbfc52ec2a85480e1b4363b743fbb2ffa2a5375"
    "main/p/python3-defaults/python3_3.13.5-1_amd64.deb c1388174dc7140e6c19a6dd39f8a0dcb38a835cd66c65644e0dded43d5223cb0"
    "main/p/python3-defaults/python3-minimal_3.13.5-1_amd64.deb a5d2371bebf3689f676e3f3bae800371bcc4392a8a51269e23d528249b399615"
    "main/p/python3.13/python3.13_3.13.5-2+deb13u5_amd64.deb 460c1fd72845516ac3ce9d1686916f3ab2e4c86f8a9746e14d26243a47e2b98d"
    "main/p/python3.13/python3.13-minimal_3.13.5-2+deb13u5_amd64.deb c1fda9e7b1bf15b2fa75a238d8a8710e142fe14c722fc650c5fc614f078c9073"
    "main/r/rpcsvc-proto/rpcsvc-proto_1.4.3-1_amd64.deb 32ac0692694f8a34cc90c895f4fc739680fb2ef0e2d4870a68833682bf1c81a3"
    "main/s/sed/sed_4.9-2+deb13u1_amd64.deb 7071270ed4f6adda55bc4f926347fb847bacaffbdee3dff917bc6006ed7e3775"
    "main/t/tar/tar_1.35+dfsg-3.1_amd64.deb 214c02e1aa291076a3147b8a4c4cae02fadf4c67df629186e3e313726faef1de"
    "main/x/xz-utils/xz-utils_5.8.1-1+deb13u1_amd64.deb 9e0a39d95373afdaad3c9dfaab3c8b0beb9a2c4654e6be10b4bcf458509a3217"
    "main/z/zlib/zlib1g_1.3.dfsg+really1.3.1-1+b1_amd64.deb 015be740d6236ad114582dea500c1d907f29e16d6db00566ca32fb68d71ac90d"
)

# And from unstable (sid) at the same moment: gcc 15, because the libc++
# port is LLVM 23's, which gcc 14 cannot compile, with the runtime libraries
# it needs. Each wants glibc 2.38 at most, which trixie's 2.41 is.
debs+=(
    "main/g/gcc-15/cpp-15-x86-64-linux-gnu_15.3.0-4_amd64.deb 1312ebf63b2157c66ed1551837329168176d49fa3153a1529f509a46e2514b3f"
    "main/g/gcc-15/g++-15-x86-64-linux-gnu_15.3.0-4_amd64.deb a8d25e6d88577ba66c2a17ee619a39d033bcf603777da4794633a92cc5ae45c4"
    "main/g/gcc-15/gcc-15-base_15.3.0-4_amd64.deb 9ba6cc41c2b8e064e265c27aa6ca0c102a923caa4258a8f3ea5627ae990f725b"
    "main/g/gcc-15/gcc-15-x86-64-linux-gnu_15.3.0-4_amd64.deb a60ed90e99181322f7834522c9768b007638429dfc4b85ae07817dd20b4e9732"
    "main/g/gcc-16/gcc-16-base_16.2.0-3_amd64.deb 7dcc6404b44ad8a9d58e83c1e987eecb4d6a24623a92438eeb11829ed1129635"
    "main/g/gcc-16/libasan8_16.2.0-3_amd64.deb c58ab5826bb190b4178d5767430b70317d28d3f39bfeab3623b11935301f8765"
    "main/g/gcc-16/libatomic1_16.2.0-3_amd64.deb 45ee39ca0d41dc7fb29e635fb1232e85d1aadb0914d79dd3585d77c832c493d3"
    "main/g/gcc-16/libcc1-0_16.2.0-3_amd64.deb aae43d7e1a8feccd61f4d9e432c69f96826a44d8df0b6c7bbe192f16b42e51fb"
    "main/g/gcc-15/libgcc-15-dev_15.3.0-4_amd64.deb 90f59487ea50b86a9ec565be97b495e2e07c9cac5067c8c96318a2fa750462bd"
    "main/g/gcc-16/libgcc-s1_16.2.0-3_amd64.deb e716dc8baad27e884c45ff83a76955047a7a9e33a80e94f9459ee996ebb800b0"
    "main/g/gcc-16/libgomp1_16.2.0-3_amd64.deb a94884d440c641499fccc658eb6845cdc7381fe3f419ab71dff8a621614122c7"
    "main/g/gcc-16/libhwasan0_16.2.0-3_amd64.deb eac63ff234fb3e9cef6120e23409841041a59b4f5b9700795ad1d7b5e0622376"
    "main/i/isl/libisl23_0.28-1_amd64.deb feec9d37a3dcdccb0c3846ff96004007c9935363df00f052e6570d35b94deb1c"
    "main/g/gcc-16/libitm1_16.2.0-3_amd64.deb de34fc9755876fe837bb3c3970d700151d0b02bf442bd2d636724692faa5c7ad"
    "main/g/gcc-16/liblsan0_16.2.0-3_amd64.deb 15bc296b18355fe9a465fa00d7460d008ac55f6d2987f02de2747b0f70d92828"
    "main/m/mpclib3/libmpc3_1.3.1-3_amd64.deb eb6113ed366a8abdea9a85040dbdfabb753a1a0637071491968cf92d4858569c"
    "main/g/gcc-16/libquadmath0_16.2.0-3_amd64.deb 885ad445d289ba26fca15fb160f0f4dec29564051e6076bd8a3e489bd29e3bce"
    "main/g/gcc-15/libstdc++-15-dev_15.3.0-4_amd64.deb bb3759f0466f3f26c099b83b9f80d7938757ce074fa4a0e041e2f615d54b4300"
    "main/g/gcc-16/libstdc++6_16.2.0-3_amd64.deb 7c9e1a89e6c0b9202699957b469c06d0b86f5c97e86c1ff912b6502ddd8793b6"
    "main/g/gcc-16/libtsan2_16.2.0-3_amd64.deb dd210da482dee72e1ab41fca36a864edda2e03a2a18d94b0cffacdb861414a04"
    "main/g/gcc-16/libubsan1_16.2.0-3_amd64.deb 3c749a6bc37bd1aca3c7ece1063032716b328f0a80013688828dcd2448dc9f46"
)

# And for foot, the Wayland terminal port: meson, which configures its
# libraries, bison, and pkg-config, from trixie with the Python modules
# meson imports.
debs+=(
    "main/b/bison/bison_3.8.2+dfsg-1+b2_amd64.deb ce6eb1f7aad18850e32d223a68f54bcbe756d74f138c8ec5460ad9d220f4d335"
    "main/p/pkgconf/libpkgconf3_1.8.1-4_amd64.deb 85087cd04e57fd4ab7d6e816d348c335047823ab60c78878336b74f07b352ca1"
    "main/m/m4/m4_1.4.19-8_amd64.deb 221b3e224708c1da2a1a88b6824380593579b2254ae7d914afa14cbd5d343334"
    "main/m/meson/meson_1.7.0-1_all.deb e92d04e60e784336883d537755d09b7dcb856809fa318a0931df3898e84ded95"
    "main/p/pkgconf/pkgconf_1.8.1-4_amd64.deb f1fcad470ca3ac80b4a0f4af6f03cd215089473996b07e508b3ee342bbb11af7"
    "main/p/pkgconf/pkgconf-bin_1.8.1-4_amd64.deb 54efef2aef4db5fcb5e0f5b7746465000c6a779b06f3686d0ec3d2a901b20aeb"
    "main/p/python-autocommand/python3-autocommand_2.2.2-3_all.deb 1e62ac22ff83466228c051c6c2402a40f521646e15b5aa54cf2391206a848bad"
    "main/p/python-importlib-metadata/python3-importlib-metadata_8.7.0-1_all.deb f895d1f4901176f5e75614bf95b98ccc2a6cd5017ba5ec727b29f281d29be1cd"
    "main/i/importlib-resources/python3-importlib-resources_6.5.2-1_all.deb 7f6c49804f85cfbe94dcb51d1c34e64795a475e69ab8f4c33bb6100e8c37fc6f"
    "main/p/python-inflect/python3-inflect_7.3.1-2_all.deb 6c10dca4d4aa0eb0b4e16e8b62b74a073a8d22be8809c3f61c56b06fdd1451f8"
    "main/j/jaraco.context/python3-jaraco.context_6.0.1-1+deb13u1_all.deb f909ec7b00dd96a1905322acba7cdd2df09747ceddeef6e426a97b33ab227f80"
    "main/p/python-jaraco.functools/python3-jaraco.functools_4.1.0-1_all.deb f5ac3e5f79b6394191d76ea7cde9255d8bae98f639886cb981444128f6545796"
    "main/j/jaraco.text/python3-jaraco.text_4.0.0-1_all.deb d5e060c9ba9d5206b53670cf2473681b45030cc6a55d18a8c6d87419a924e502"
    "main/m/more-itertools/python3-more-itertools_10.7.0-1_all.deb 385d785fa84261838732ec97627b4e95ec7399fc449d8a2aaaef66fb5f961c25"
    "main/s/setuptools/python3-pkg-resources_78.1.1-0.1_all.deb cf8749251172abc945216cf878367f614cf91d3d9a1043ba3bacc5a74e8cc1a8"
    "main/s/setuptools/python3-setuptools_78.1.1-0.1_all.deb 90c4bc8e311873e5b571b407ce6b1fd38195dff77f9de056301a0f950e6df5a5"
    "main/p/python-typeguard/python3-typeguard_4.4.2-1_all.deb 81e512101d7ab94e73646b9a8e4fb9b948836e3172dc5719ad3acbcde65bca01"
    "main/p/python-typing-extensions/python3-typing-extensions_4.13.2-1_all.deb b67662f211c35c17c74a0725bc7c8ffc3b126c650e2a2525593adee06f2bbf69"
    "main/p/python-zipp/python3-zipp_3.21.0-1_all.deb 6cd0f768a5e5c574c7ae4b9d56603f8399b31de732167007e42fc17decc5cc6a"
)

# foot's build wants wayland-scanner at 1.24.0, the version of the wayland
# library it builds, and neither trixie (1.23.1) nor sid at the snapshot above
# (1.26.0) has it: it comes, with the libxml2 it links, from sid as
# snapshot.debian.org had it at 20250901T000000Z, pinned by that day's
# Packages file. meson finds the scanner through pkg-config, so
# libwayland-dev comes too, for wayland-scanner.pc, with what it depends on.
older=${DEBIAN_MIRROR_2025:-https://snapshot.debian.org/archive/debian/20250901T000000Z}
older_debs=(
    "main/libf/libffi/libffi-dev_3.4.8-2_amd64.deb 76b2b80193a656733e1408ebec5371334be5fd48eb5f50ab99d6baf6081d0f1b"
    "main/w/wayland/libwayland-bin_1.24.0-2+b1_amd64.deb 969e0faec61659106a03fb8f1757e1c524c08c60960e143c5d1e531454ece037"
    "main/w/wayland/libwayland-client0_1.24.0-2+b1_amd64.deb 3e4ad51dd45e5ffecf02ec7662bcc457a973e7b4272ee4b87bc318fd5d8da2fe"
    "main/w/wayland/libwayland-cursor0_1.24.0-2+b1_amd64.deb cfca80308161c17e198a2eb9a662e982dc94a742df4a3754503393ed47ca1d62"
    "main/w/wayland/libwayland-dev_1.24.0-2+b1_amd64.deb 9bcd6324c491d6b919c30640ad10e7b4cf0398e3af89499c5adb4f98e63cdd34"
    "main/w/wayland/libwayland-egl1_1.24.0-2+b1_amd64.deb c4f7ebb24910512e9487b02c717e5e0c889f55656ad49e14e3d2a909d5a12f1e"
    "main/w/wayland/libwayland-server0_1.24.0-2+b1_amd64.deb 1bf4c1ef333ebd271446f25e8de02c7bf110684cc126ac7a67820319fc058f19"
    "main/libx/libxml2/libxml2-16_2.14.5+dfsg-0.2_amd64.deb ab5e5c7fe002037cd67f7dbe68f45451fe50f66be563fcf2482408e3cdc278a4"
)

# Path under the dist server and SHA-256 of each Rust component.
components=(
    "dist/2026-07-16/rustc-1.97.1-x86_64-unknown-linux-gnu.tar.xz 9819d0a32d56bd339585319c80260e332779f5541fd66838ab7e016d6c814819"
    "dist/2026-07-16/rust-std-1.97.1-x86_64-unknown-linux-gnu.tar.xz 1c1e704ae80126b7de34f72ea2825f7fd01736dec20732faed47374b95282fba"
    "dist/2026-07-16/rust-std-1.97.1-x86_64-unknown-none.tar.xz 24e213f586ecb1811a11bd40dbb53690fbae469cce89dc60f7cf20eaeaaeab29"
    "dist/2026-07-16/rust-std-1.97.1-x86_64-unknown-uefi.tar.xz 35f18a13185697e26540ef139de4408689fa77fb6427b355e0a9954d632f20fc"
    "dist/2026-07-16/rust-std-1.97.1-aarch64-unknown-none-softfloat.tar.xz 02eb0d235d8f3af63ce896178a96fcf0e78e05b5241e5ddc92d6a07e28bf5e0e"
    "dist/2026-07-16/rust-std-1.97.1-aarch64-unknown-uefi.tar.xz 90fd767018a4800c764bf06212eb96dfd17f0f7da1c2a070ee30d66c0057ac79"
    "dist/2026-07-16/rust-std-1.97.1-armv7a-none-eabi.tar.xz d9afee2a85c5a38a07ba5277ab3b1fe8560a6bfd06301256bdc57b79442f270d"
    "dist/2026-07-16/rust-std-1.97.1-armv7-unknown-linux-musleabi.tar.xz ad50b2c455548ca7a7660251a6b8ea09156182583204053da9ea000b50723ab6"
    "dist/2026-07-16/rust-std-1.97.1-x86_64-unknown-linux-musl.tar.xz 51d83178680556f73a5fa8ad865b76a1ff541867445c00fc65dc67246bc2de66"
    "dist/2026-07-16/rust-std-1.97.1-aarch64-unknown-linux-musl.tar.xz 49ff0879d94e2e8e86d5e85eb15a9215943e8c78b51363d6553443598cab5d31"
    "dist/2026-07-16/cargo-1.97.1-x86_64-unknown-linux-gnu.tar.xz e1be5f5ff7f7f80ca506fb65770b759edbdc6d303781ed71c5de8ec8a8394779"
    # What working on the tree from within Ferrix needs beside the compiler
    # (docs/DEV-ON-FERRIX.md): `cargo xtask check` starts with `cargo fmt`
    # and runs clippy, and an editor's language server reads the standard
    # library's source.
    "dist/2026-07-16/rustfmt-1.97.1-x86_64-unknown-linux-gnu.tar.xz 907fe97d6afbde1eca1b34c992c76e1406d422e2e6f137813d382acec7eb4d14"
    "dist/2026-07-16/clippy-1.97.1-x86_64-unknown-linux-gnu.tar.xz 3441df8fb54db985f8c8a3e8356b8874a3f92cc8cca8565cfe36f1dc15935e72"
    "dist/2026-07-16/rust-analyzer-1.97.1-x86_64-unknown-linux-gnu.tar.xz 4d6b6055bea92ed2b2e53a35577d9b43dd4d0c6234be72044d3097b7d3f16572"
    "dist/2026-07-16/rust-src-1.97.1.tar.xz e9a1e616d04c6845895c827a178b9227f7c7199f3f4a80af81ab3aff7b80156b"
)

# The same packages and components for an AArch64 host, in place of the lists
# above.
if [ "$arch" = arm64 ]; then
    . "$(dirname "$0")/rustc-sysroot-arm64.sh"
fi

for tool in curl sha256sum dpkg-deb tar mkfs.btrfs; do
    command -v "$tool" > /dev/null || { echo "fetch-rustc-sysroot: $tool is not installed" >&2; exit 1; }
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
        || { echo "fetch-rustc-sysroot: $file does not match its pinned checksum" >&2; rm -f "$file"; exit 1; }
    printf '%s\n' "$file"
}

mkdir -p "$out/pool"
tree="$out/tree"
rm -rf "$tree"
mkdir -p "$tree"

for entry in "${debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$debian/pool" "$sum")" "$tree"
done
for entry in "${older_debs[@]}"; do
    read -r path sum <<< "$entry"
    dpkg-deb -x "$(fetch "$path" "$older/pool" "$sum")" "$tree"
done

# Each component carries an install.sh and a `components` list; its files
# are the directories beside them, laid out from the prefix down.
unpacked=$(mktemp -d)
trap 'rm -rf "$unpacked"' EXIT
for entry in "${components[@]}"; do
    read -r path sum <<< "$entry"
    tar -xf "$(fetch "$path" "$rust" "$sum")" -C "$unpacked"
done
mkdir -p "$tree/rust"
for component in "$unpacked"/*/*/; do
    case "$component" in
        */rustc/ | */cargo/ | */rust-std-*/ \
            | */rustfmt-preview/ | */clippy-preview/ | */rust-analyzer-preview/ | */rust-src/)
            cp -a "$component". "$tree/rust/" ;;
    esac
done

# helix: its tarball's one directory becomes `helix/`.
read -r path sum <<< "$helix"
mkdir -p "$tree/helix"
tar -xf "$(fetch "$path" "$helix_releases" "$sum")" -C "$tree/helix" --strip-components=1

# What Debian's postinst scripts and alternatives would have made. `cc` is
# the name rustc runs.
ln -sf "$gnu-gcc-15" "$tree/usr/bin/cc"
ln -sf "$gnu-gcc-15" "$tree/usr/bin/gcc"
# gawk installs itself as `gawk`; `awk` is an alternative. g++ is named by
# its target, as gcc is.
ln -sf gawk "$tree/usr/bin/awk"
ln -sf "$gnu-g++-15" "$tree/usr/bin/g++"
ln -sf "$gnu-g++-15" "$tree/usr/bin/c++"
ln -sf python3 "$tree/usr/bin/python"

# Documentation and manuals, which nothing runs.
rm -rf "$tree/usr/share/doc" "$tree/usr/share/man" "$tree/usr/share/lintian" "$tree/rust/share"

test -x "$tree/rust/bin/rustc" || { echo "fetch-rustc-sysroot: no rustc in the tree" >&2; exit 1; }
test -x "$tree/rust/bin/cargo" || { echo "fetch-rustc-sysroot: no cargo in the tree" >&2; exit 1; }
for tool in rustfmt cargo-fmt clippy-driver cargo-clippy rust-analyzer; do
    test -x "$tree/rust/bin/$tool" || { echo "fetch-rustc-sysroot: no $tool in the tree" >&2; exit 1; }
done
test -x "$tree/helix/hx" && test -d "$tree/helix/runtime/grammars" \
    || { echo "fetch-rustc-sysroot: no helix and its runtime in the tree" >&2; exit 1; }
test -e "$tree/rust/lib/rustlib/src/rust/library/core/src/lib.rs" \
    || { echo "fetch-rustc-sysroot: no standard library source in the tree" >&2; exit 1; }
test -x "$tree/rust/lib/rustlib/$triple/bin/rust-lld" \
    || { echo "fetch-rustc-sysroot: no rust-lld in the tree" >&2; exit 1; }
for target in x86_64-unknown-none x86_64-unknown-uefi aarch64-unknown-none-softfloat     aarch64-unknown-uefi armv7a-none-eabi armv7-unknown-linux-musleabi     x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
    test -d "$tree/rust/lib/rustlib/$target/lib" \
        || { echo "fetch-rustc-sysroot: no standard library for $target in the tree" >&2; exit 1; }
done
for tool in "usr/libexec/gcc/$gnu/15/cc1" "usr/libexec/gcc/$gnu/15/cc1plus" \
    usr/bin/cmake usr/bin/ninja usr/bin/as usr/bin/ld usr/bin/ar usr/bin/make \
    usr/bin/bash usr/bin/dash usr/bin/sed usr/bin/gawk usr/bin/tar \
    usr/bin/meson usr/bin/bison usr/bin/m4 usr/bin/pkg-config usr/bin/wayland-scanner; do
    test -e "$tree/$tool" || { echo "fetch-rustc-sysroot: no $tool in the tree" >&2; exit 1; }
done
test -e "$tree/usr/include/linux/types.h" \
    || { echo "fetch-rustc-sysroot: no kernel UAPI headers in the tree" >&2; exit 1; }
test -e "$tree/$loader" \
    || { echo "fetch-rustc-sysroot: no dynamic loader at $loader" >&2; exit 1; }

# Room to spare for what the compile writes when it runs on the volume
# itself, and a size that does not depend on how mkfs rounds.
size=$(( $(du -sm "$tree" | cut -f1) + 1024 ))
image="$out/rustc.img"
rm -f "$image"
truncate -s "${size}M" "$image"
mkfs.btrfs -q --rootdir "$tree" "$image"
# The tree stays beside the image, so the same files can be tried on the host.
echo "rustc sysroot: $image (${size} MiB), unpacked in $tree"
