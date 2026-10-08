#!/bin/sh
# build.sh: lmbench and SQLite's speedtest1 as static ARMv7-A programs on
# musl, for BOARD-BENCH's level 4 ("Programs", item B5). Linux 7.2.9 and
# Ferrix on the DK1 run the identical binaries; the result is a ratio to Linux.
#
#   build.sh [--work DIR] [--out DIR] [--jobs N]
#
# Fetches what pins.sh names into DIR/dl, checks every checksum (and musl's
# signature where gpg exists), builds musl into DIR/sysroot, then the
# programs, and installs them with the run scripts, SHA256SUMS and
# BUILD-INFO into the output directory. Nothing is vendored; the only things
# added to lmbench are two empty headers (see build_lmbench).
#
# Needs a Linux host with arm-linux-gnueabihf-gcc (nazuna: Ubuntu's 15.2),
# make, curl, tar, unzip (or python3), sha256sum and openssl or sha3sum.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/pins.sh"

# The instruction set, one variable for every C bench on the board. Ferrix's
# armv7a user code is A32 (B3, 2026-10-08: target armv7a-none-eabi, only $a
# mapping symbols in ipc-bench), so -marm. Ubuntu's gcc defaults to Thumb-2:
# never leave this to the default.
BENCH_ISA=${BENCH_ISA:--marm}
BENCH_CROSS=${BENCH_CROSS:-arm-linux-gnueabihf-}
BENCH_CFLAGS="-mcpu=cortex-a7 -mfpu=neon-vfpv4 -mfloat-abi=hard $BENCH_ISA -O2"
# Ubuntu's gcc turns these on by default. They are turned off by name so a
# stock gcc 15.2 elsewhere compiles the same code (musl has no fortify layer).
BENCH_DEFAULTS_OFF="-fno-stack-protector -fno-stack-clash-protection -U_FORTIFY_SOURCE -fno-pie"
BENCH_LDFLAGS="-static -no-pie"

WORK=$HOME/.local/share/ferrix/board-bench/posix-work
OUT=$HOME/.local/share/ferrix/board-bench/posix
JOBS=4
while [ $# -gt 0 ]; do
	case $1 in
	--work) WORK=$2; shift 2 ;;
	--out) OUT=$2; shift 2 ;;
	--jobs) JOBS=$2; shift 2 ;;
	*) echo "build.sh: unknown argument $1" >&2; exit 2 ;;
	esac
done

# The lmbench programs BOARD-BENCH level 4 runs. hello is what lat_proc
# execs; it is copied to /tmp/hello by run-lmbench.sh, as lmbench's own
# scripts/lmbench does.
LMBENCH_PROGRAMS="hello lat_syscall lat_pipe lat_unix lat_ctx lat_proc lat_sig lat_pagefault lat_mmap bw_pipe bw_unix lat_mem_rd bw_mem"

say() { echo "build.sh: $*"; }
die() { echo "build.sh: $*" >&2; exit 1; }

sha256_of() { sha256sum "$1" | cut -d' ' -f1; }

sha3_of() {
	if command -v openssl >/dev/null 2>&1; then
		openssl dgst -sha3-256 "$1" | sed 's/.*= *//'
	elif command -v sha3sum >/dev/null 2>&1; then
		sha3sum -a 256 "$1" | cut -d' ' -f1
	else
		die "no SHA3-256 tool (openssl or sha3sum)"
	fi
}

# fetch <dir> <url> <sha256>: download into WORK/dl/<dir>/, a directory of
# its own, unless a file there already has the right sha256.
fetch() {
	dir=$WORK/dl/$1
	file=$dir/$(basename "$2")
	mkdir -p "$dir"
	if [ -f "$file" ] && [ "$(sha256_of "$file")" = "$3" ]; then
		say "have $(basename "$file")"
	else
		say "fetching $2"
		rm -f "$file.part"
		curl -fsSL --retry 3 -o "$file.part" "$2"
		got=$(sha256_of "$file.part")
		[ "$got" = "$3" ] || die "$2: sha256 $got, pinned $3"
		mv "$file.part" "$file"
	fi
	FETCHED=$file
}

verify_musl_signature() {
	if ! command -v gpg >/dev/null 2>&1; then
		say "no gpg: musl checked by its pinned sha256 only"
		MUSL_SIG="sha256 pin only (no gpg on the build host)"
		return
	fi
	sigdir=$WORK/dl/musl-sig
	mkdir -p "$sigdir"
	curl -fsSL --retry 3 -o "$sigdir/musl.tar.gz.asc" "$MUSL_SIG_URL"
	curl -fsSL --retry 3 -o "$sigdir/musl.pub" "$MUSL_KEY_URL"
	home=$sigdir/gnupg
	rm -rf "$home"
	mkdir -m 700 "$home"
	gpg --homedir "$home" --quiet --import "$sigdir/musl.pub" 2>/dev/null
	fpr=$(gpg --homedir "$home" --with-colons --fingerprint | awk -F: '$1 == "fpr" { print $10; exit }')
	[ "$fpr" = "$MUSL_KEY_FPR" ] || die "musl.pub has key $fpr, pinned $MUSL_KEY_FPR"
	gpg --homedir "$home" --status-fd 1 --verify "$sigdir/musl.tar.gz.asc" "$1" 2>/dev/null |
		grep -q "^\[GNUPG:\] VALIDSIG $MUSL_KEY_FPR " ||
		die "musl $MUSL_VERSION: bad signature"
	say "musl signature good, key $MUSL_KEY_FPR"
	MUSL_SIG="good signature by $MUSL_KEY_FPR"
}

# unzip_to <zip> <dir> [member...]
unzip_to() {
	zip=$1
	dir=$2
	shift 2
	mkdir -p "$dir"
	if command -v unzip >/dev/null 2>&1; then
		unzip -q -o "$zip" "$@" -d "$dir"
	else
		python3 -I -c '
import sys, zipfile
z = zipfile.ZipFile(sys.argv[1])
z.extractall(sys.argv[2], sys.argv[3:] or None)
' "$zip" "$dir" "$@"
	fi
}

build_musl() {
	fetch musl "$MUSL_URL" "$MUSL_SHA256"
	tarball=$FETCHED
	verify_musl_signature "$tarball"
	rm -rf "$WORK/src/musl-$MUSL_VERSION" "$WORK/build/musl" "$WORK/sysroot"
	mkdir -p "$WORK/src" "$WORK/build/musl"
	tar -xzf "$tarball" -C "$WORK/src"
	say "building musl $MUSL_VERSION"
	(
		cd "$WORK/build/musl"
		"$WORK/src/musl-$MUSL_VERSION/configure" \
			--target=arm-linux-gnueabihf \
			--prefix="$WORK/sysroot" \
			--disable-shared \
			--enable-wrapper=gcc \
			CROSS_COMPILE="$BENCH_CROSS" \
			CC="${BENCH_CROSS}gcc" \
			CFLAGS="$BENCH_CFLAGS $BENCH_DEFAULTS_OFF" >configure.log 2>&1 ||
			{ tail -20 configure.log; exit 1; }
		make -j"$JOBS" >make.log 2>&1 || { tail -30 make.log; exit 1; }
		make install >install.log 2>&1 || { tail -30 install.log; exit 1; }
	)
	MUSL_GCC=$WORK/sysroot/bin/musl-gcc
	[ -x "$MUSL_GCC" ] || die "musl-gcc was not installed"
}

# lmbench is built by its own scripts/build, which probes the compiler and
# runs its Makefile, so the build is lmbench's and no source file changes.
# Two things are added outside its tree:
#  - empty <rpc/rpc.h> and <rpc/types.h>, which bench.h includes for
#    lat_rpc's sake; musl has no Sun RPC, and neither lat_rpc nor anything in
#    lmbench.a that the programs here link uses it (NO_PORTMAPPER is set);
#  - -std=gnu89, the dialect lmbench is written in (unprototyped
#    declarations, implicit int in the probes), which gcc 15's default
#    gnu23 rejects.
build_lmbench() {
	fetch lmbench "$LMBENCH_URL" "$LMBENCH_SHA256"
	tarball=$FETCHED
	[ "$(sha1sum "$tarball" | cut -d' ' -f1)" = "$LMBENCH_SHA1" ] || die "lmbench: sha1 differs from SourceForge's"
	rm -rf "$WORK/build/lmbench"
	mkdir -p "$WORK/build/lmbench/shim/rpc"
	tar -xzf "$tarball" -C "$WORK/build/lmbench"
	for h in rpc types; do
		echo "/* Empty: lmbench's bench.h includes this for lat_rpc, which is not built (B5). */" \
			>"$WORK/build/lmbench/shim/rpc/$h.h"
	done
	os=armv7a-linux-musl
	targets=
	for p in $LMBENCH_PROGRAMS; do
		targets="$targets ../bin/$os/$p"
	done
	say "building lmbench $LMBENCH_VERSION:$LMBENCH_PROGRAMS"
	(
		cd "$WORK/build/lmbench/lmbench-$LMBENCH_VERSION/src"
		# shellcheck disable=SC2086 # targets is a list
		env CC="$MUSL_GCC" MAKE=make OS="$os" \
			CFLAGS="$BENCH_CFLAGS $BENCH_DEFAULTS_OFF -std=gnu89 -I$WORK/build/lmbench/shim $BENCH_LDFLAGS" \
			sh ../scripts/build $targets >build.log 2>&1 || { tail -40 build.log; exit 1; }
	)
	LMBENCH_BIN=$WORK/build/lmbench/lmbench-$LMBENCH_VERSION/bin/$os
	# Ours, not lmbench's: checks that lat_proc's exec runs hello at all,
	# since lat_proc never reads its child's status (exec-check.c).
	# shellcheck disable=SC2086 # flag lists
	"$MUSL_GCC" $BENCH_CFLAGS $BENCH_DEFAULTS_OFF -o "$LMBENCH_BIN/exec-check" \
		"$HERE/exec-check.c" $BENCH_LDFLAGS
}

# speedtest1 with the base options SQLite's own test/speedtest.tcl compiles
# it with: no extension loading, no threads. The allocator is musl's malloc,
# so the kernel's brk/mmap paths are part of what is measured.
SQLITE_DEFINES="-DSQLITE_OMIT_LOAD_EXTENSION -DSQLITE_THREADSAFE=0"
build_speedtest1() {
	fetch sqlite-amalgamation "$SQLITE_AMALG_URL" "$SQLITE_AMALG_SHA256"
	amalg=$FETCHED
	[ "$(sha3_of "$amalg")" = "$SQLITE_AMALG_SHA3" ] || die "$amalg: SHA3-256 differs from sqlite.org's"
	fetch sqlite-src "$SQLITE_SRC_URL" "$SQLITE_SRC_SHA256"
	src=$FETCHED
	[ "$(sha3_of "$src")" = "$SQLITE_SRC_SHA3" ] || die "$src: SHA3-256 differs from sqlite.org's"
	b=$WORK/build/sqlite
	rm -rf "$b"
	unzip_to "$amalg" "$b"
	unzip_to "$src" "$b" "$SQLITE_SRC_DIR/test/speedtest1.c" "$SQLITE_SRC_DIR/manifest.uuid"
	[ "$(cat "$b/$SQLITE_SRC_DIR/manifest.uuid")" = "$SQLITE_CHECKIN" ] || die "sqlite source is not check-in $SQLITE_CHECKIN"
	say "building speedtest1 (SQLite $SQLITE_VERSION)"
	# shellcheck disable=SC2086 # flag lists
	"$MUSL_GCC" $BENCH_CFLAGS $BENCH_DEFAULTS_OFF $SQLITE_DEFINES \
		-I"$b/$SQLITE_AMALG_DIR" -o "$b/speedtest1" \
		"$b/$SQLITE_SRC_DIR/test/speedtest1.c" "$b/$SQLITE_AMALG_DIR/sqlite3.c" \
		$BENCH_LDFLAGS -lm >"$b/build.log" 2>&1 || { tail -30 "$b/build.log"; exit 1; }
}

install_outputs() {
	rm -rf "$OUT/bin"
	mkdir -p "$OUT/bin"
	for p in $LMBENCH_PROGRAMS exec-check; do
		cp "$LMBENCH_BIN/$p" "$OUT/bin/$p"
	done
	cp "$WORK/build/sqlite/speedtest1" "$OUT/bin/speedtest1"
	cp "$HERE/run-lmbench.sh" "$HERE/run-speedtest1.sh" "$OUT/"
	chmod 755 "$OUT/run-lmbench.sh" "$OUT/run-speedtest1.sh"
	for f in "$OUT"/bin/*; do
		# Every program must be a static ARM executable: no interpreter.
		if "${BENCH_CROSS}readelf" -l "$f" | grep -q INTERP; then
			die "$f is dynamically linked"
		fi
	done
	tree=unknown
	if git -C "$HERE" rev-parse HEAD >/dev/null 2>&1; then
		tree=$(git -C "$HERE" rev-parse HEAD)
		git -C "$HERE" diff --quiet HEAD -- . || tree="$tree (with uncommitted changes)"
	fi
	{
		echo "built: $(date -u +%Y-%m-%dT%H:%M:%SZ) on $(uname -n)"
		echo "script tree: $tree"
		echo "compiler: $("${BENCH_CROSS}gcc" --version | head -1)"
		echo "isa: $BENCH_ISA"
		echo "cflags: $BENCH_CFLAGS $BENCH_DEFAULTS_OFF"
		echo "ldflags: $BENCH_LDFLAGS"
		echo "libc: musl $MUSL_VERSION static, sha256 $MUSL_SHA256, $MUSL_SIG"
		echo "lmbench: $LMBENCH_VERSION, sha256 $LMBENCH_SHA256, extra cflags -std=gnu89, empty rpc/rpc.h and rpc/types.h"
		echo "sqlite: $SQLITE_VERSION (check-in $SQLITE_CHECKIN), amalgamation sha3-256 $SQLITE_AMALG_SHA3, defines $SQLITE_DEFINES"
		echo "thumb mapping symbols (\$t) per program, from libgcc.a where not zero:"
		for f in "$OUT"/bin/*; do
			n=$("${BENCH_CROSS}readelf" -s "$f" | grep -c ' \$t' || true)
			echo "  $(basename "$f") $n"
		done
	} >"$OUT/BUILD-INFO"
	(cd "$OUT" && sha256sum bin/* run-lmbench.sh run-speedtest1.sh >SHA256SUMS)
	say "installed into $OUT"
	(cd "$OUT" && ls -l bin && cat SHA256SUMS)
}

command -v "${BENCH_CROSS}gcc" >/dev/null 2>&1 || die "no ${BENCH_CROSS}gcc"
mkdir -p "$WORK" "$OUT"
build_musl
build_lmbench
build_speedtest1
install_outputs
