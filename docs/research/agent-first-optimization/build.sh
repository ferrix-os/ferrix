#!/usr/bin/env bash
# Build build/paper.pdf from paper.md: pandoc (citeproc) to LaTeX, then
# tectonic to PDF. The tools are static release binaries outside the tree
# (notes.md, "Building"); PAPER_TOOLS overrides where they are.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tools=${PAPER_TOOLS:-$HOME/.local/share/ferrix/paper-tools}
pandoc=$tools/pandoc-3.12/bin/pandoc
tectonic=$tools/tectonic
for t in "$pandoc" "$tectonic"; do
  [ -x "$t" ] || { echo "build.sh: $t is missing (notes.md, Building)" >&2; exit 1; }
done
cd "$here"
mkdir -p build
# --fail-if-warnings stops on any citeproc warning, such as an unknown key.
"$pandoc" paper.md --standalone --citeproc --fail-if-warnings \
  --to latex --output build/paper.tex
"$tectonic" -X compile --keep-logs --outdir build \
  -Z search-path="$here" build/paper.tex
# No citation may render as a question mark or a bare key.
if command -v pdftotext > /dev/null; then
  text=$(pdftotext build/paper.pdf -)
  if grep -q -E '\?\?\?|\[@' <<< "$text"; then
    echo "build.sh: an unresolved citation is in the PDF" >&2
    exit 1
  fi
fi
echo "build.sh: wrote $here/build/paper.pdf"
