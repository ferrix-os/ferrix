# pins.sh: what build.sh fetches, each with the checksum it must match.
# Sourced by build.sh; POSIX sh. Checked 2026-10-08 (os07-b5).

# musl 1.2.5, the release tarball. Its signature (.asc) verifies with musl's
# key, fingerprint MUSL_KEY_FPR, which keyserver.ubuntu.com lists under
# "musl libc <musl@libc.org>". The sha256 below is the signed tarball's,
# and the same as ~/.local/share/ferrix/ferrousli-ref/musl-1.2.5.tar.gz on
# nazuna, whose unpacked tree equals the tarball's (diff -r, 2026-10-08).
MUSL_VERSION=1.2.5
MUSL_URL=https://musl.libc.org/releases/musl-1.2.5.tar.gz
MUSL_SHA256=a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4
MUSL_SIG_URL=https://musl.libc.org/releases/musl-1.2.5.tar.gz.asc
MUSL_KEY_URL=https://musl.libc.org/musl.pub
MUSL_KEY_FPR=836489290BB6B70F99FFDA0556BCDB593020450F

# lmbench 3.0-a9, the original release (Larry McVoy and Carl Staelin), not a
# fork: the licence forbids publishing results of modified benchmarks
# (README.md, "Licence"). SourceForge publishes the sha1 and md5 below for
# this file; the sha256 is computed from the file those match.
LMBENCH_VERSION=3.0-a9
LMBENCH_URL=https://downloads.sourceforge.net/project/lmbench/development/lmbench-3.0-a9/lmbench-3.0-a9.tgz
LMBENCH_SHA256=cbd5777d15f44eab7666dcac418054c3c09df99826961a397d9acf43d8a2a551
LMBENCH_SHA1=8c11ca459d399c38649caa137752caec28a78956
LMBENCH_MD5=b3351a3294db66a72e2864a199d37cbf

# SQLite 3.53.4, the latest release on 2026-10-08: the amalgamation, and
# test/speedtest1.c from the same release's source archive. The SHA3-256
# values are the ones sqlite.org's download page lists; the sha256 values are
# computed from the files those match, for a host with no SHA3 tool.
SQLITE_VERSION=3.53.4
SQLITE_AMALG_URL=https://sqlite.org/2026/sqlite-amalgamation-3530400.zip
SQLITE_AMALG_SHA3=628a44cfe82c66aed1ccbbe85a562d2e33ebe64b3288981ed76285612227934e
SQLITE_AMALG_SHA256=1e71ddf93849c6a6ecf58b827c0692073d2dd7ee40196158068f7b29f422e87d
SQLITE_AMALG_DIR=sqlite-amalgamation-3530400
SQLITE_SRC_URL=https://sqlite.org/2026/sqlite-src-3530400.zip
SQLITE_SRC_SHA3=b834d474b9b393d85a9e3ee4cc11f1329e007e9376a424ee740796f5c4bda3a8
SQLITE_SRC_SHA256=d18fa15aec74d8c17e1463f861095adc01b5ad190256acb4f91d22f0368d232b
SQLITE_SRC_DIR=sqlite-src-3530400
# The release's check-in (the archive's manifest.uuid).
SQLITE_CHECKIN=bf7c7f30031888f4e796e429ab3978879485813aaca6f641c7b33e4e09459bcc
