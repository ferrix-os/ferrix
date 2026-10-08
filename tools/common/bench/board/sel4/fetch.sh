#!/bin/bash
# Fetch seL4, its tools and libraries at the pins the x86-64 runs use
# (sel4bench-manifest 80add415 and sel4test-manifest 555edd2b, both with seL4
# c6ce4d2a), and apply the STM32MP1 platform and the matched root task.
#
#   ./fetch.sh [root]     root defaults to ~/.local/share/ferrix/board-bench/sel4
#
# The checkout goes to <root>/src. A repository already there is moved to its
# pin and reset to it, so the patches apply to a clean tree each time; nothing
# outside <root>/src is touched.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=${1:-$HOME/.local/share/ferrix/board-bench/sel4}
src=$root/src
mkdir -p "$src"
cd "$src"

get() { # url path rev
  if [ ! -e "$2/.git" ]; then
    if [ "$2" = projects/musllibc ]; then
      # seL4's musllibc build copies the tree and runs `rm -f src/.git`, which
      # fails on a directory: keep the repository outside, as a gitdir file.
      mkdir -p "$root/gitdirs"
      git clone -q --filter=blob:none --separate-git-dir="$root/gitdirs/musllibc.git" "$1" "$2"
    else
      git clone -q --filter=blob:none "$1" "$2"
    fi
  fi
  git -C "$2" checkout -q --detach "$3"
  git -C "$2" reset -q --hard "$3"
  git -C "$2" clean -q -fdx
  echo "$2 $(git -C "$2" rev-parse --short=12 HEAD)"
}

G=https://github.com/seL4
# The kernel and everything both manifests share.
get $G/seL4.git                kernel                      c6ce4d2a0c334cc9365b2cc41ff0126d75c0ea3c
get $G/seL4_tools.git          tools/seL4                  f1f63d93301cf491abc2d38ffb1a97803c218b31
get $G/seL4_libs.git           projects/seL4_libs          262a34dcb2f3285be01df9e5404d911132d567a6
get $G/util_libs.git           projects/util_libs          8dd23f736664fe61aefc25ea45ffff8127bcdf2b
get $G/musllibc.git            projects/musllibc           b0005f86fecbd6d0257b15363a5b013446914265
get $G/sel4runtime.git         projects/sel4runtime        86489cf6efab9f314964e79468c036e9035394c7
get $G/sel4_projects_libs.git  projects/sel4_projects_libs fe2647c2582cd22a83e07491bb281ce061324b81
get https://github.com/nanopb/nanopb nanopb                cad3c18ef15a663e30e3e43e3a752b66378adec1
# sel4bench-manifest 80add415.
get $G/projects_libs.git       projects/projects_libs      dfee9caa847c4cc1af8ed11d978a964cb9ec49be
get $G/sel4bench.git           projects/sel4bench          18f9d5f079bc23551eccbdc2202cae95c72c69d7
# sel4test-manifest 555edd2b (2026-10-01): the same kernel and libraries.
get $G/sel4test.git            projects/sel4test           b00d84fca8890e34f6007f104372fec5e82a23d1

# sel4bench's settings look for nanopb at nanopb/, sel4test's at tools/nanopb/.
ln -sfn ../nanopb tools/nanopb

# The platform and the root task, one patch per repository. NO_PATCHES=1
# leaves the pinned trees clean (for making the patches).
[ "${NO_PATCHES:-0}" = 1 ] && exit 0
apply() { # dir patch
  git -C "$1" apply --whitespace=nowarn "$here/patches/$2"
  echo "applied $2"
}
apply kernel                   seL4-stm32mp1.patch
apply tools/seL4               seL4_tools-stm32mp1.patch
apply projects/util_libs       util_libs-stm32mp1.patch
apply projects/seL4_libs       seL4_libs-stm32mp1.patch
apply projects/sel4bench       sel4bench-stm32mp1-sel4rt.patch
# sel4rt includes the timing contract unchanged.
cp "$here/../common/board-bench.h" projects/sel4bench/apps/ipc/src/board-bench.h
echo "copied board-bench.h ($(sha256sum < "$here/../common/board-bench.h" | cut -c1-12))"
