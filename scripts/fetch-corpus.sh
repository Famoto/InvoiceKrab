#!/bin/sh
# Fetches third-party test invoices into testfiles/corpus/ for the
# transformation correctness harness
# (crates/einvoice-interfaces/tests/transformation_correctness.rs), which runs
# them when KRAB_CORPUS_DIR=testfiles/corpus is set:
#
#   scripts/fetch-corpus.sh && KRAB_CORPUS_DIR=testfiles/corpus cargo test
#
# They are not part of the repository: their publishers' terms are their own
# (see testfiles/corpus/SOURCES.md). Nothing else needs them.
#
# Every file is downloaded from its upstream repository pinned to a commit and
# verified against testfiles/corpus/SHA256SUMS; a file already present with
# the right checksum is kept. Safe to re-run.
#
# Usage: scripts/fetch-corpus.sh   (from anywhere)
set -eu

dir="$(cd "$(dirname "$0")/.." && pwd)/testfiles/corpus"
# KoSIT XRechnung test suite, tag v2026-08-31 (Apache-2.0).
kosit='https://raw.githubusercontent.com/itplr-kosit/xrechnung-testsuite/44c3905c69ca236aa3b94c04be1b7e9bd066ba03/src/test'
# OpenPeppol BIS Billing 3.0, tag 3.0.21 (no license declared).
peppol='https://raw.githubusercontent.com/OpenPEPPOL/peppol-bis-invoice-3/5df0eff7c8efafd7c79c0dacce55082d175db759/rules'

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

url_of() {
    case "$1" in
        kosit/*) echo "$kosit/${1#kosit/}" ;;
        peppol/*) echo "$peppol/${1#peppol/}" ;;
        *) echo "no source known for $1" >&2; return 1 ;;
    esac
}

fetched=0
while read -r sum path; do
    path="${path#./}"
    file="$dir/$path"
    if [ -f "$file" ] && [ "$(sha256 "$file")" = "$sum" ]; then
        continue
    fi
    url="$(url_of "$path")"
    mkdir -p "$(dirname "$file")"
    curl -fsSL --retry 3 -o "$file.part" "$url"
    actual="$(sha256 "$file.part")"
    if [ "$actual" != "$sum" ]; then
        rm -f "$file.part"
        echo "checksum mismatch for $path from $url: got $actual, expected $sum" >&2
        exit 1
    fi
    mv "$file.part" "$file"
    fetched=$((fetched + 1))
done < "$dir/SHA256SUMS"

echo "corpus: $fetched fetched, all verified in $dir"
