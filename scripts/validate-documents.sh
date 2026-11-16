#!/usr/bin/env bash
# Validates every source document as written by every spoke with
# einvoice-validate (crate einvoice-cli): the XML schema, then the official
# Schematron rules of the spoke. The sources are testfiles/*.xml and, once
# scripts/fetch-corpus.sh has fetched them, the documents under
# testfiles/corpus/. A source that is invalid itself is skipped, and so is one
# no spoke reads or the spoke refuses to write (the correctness test covers
# those). FatturaPA has no official Schematron, so its output is not validated
# here.
#
# Needs einvoice-validate on PATH and its fetched rule sets:
#
#   cargo install --locked einvoice-cli --version 0.4.0
#   einvoice-validate rules fetch xrechnung peppol factur-x
#
# CI and the pre-commit hook run it. Exits 1 if a written document is invalid.
#
# Usage: scripts/validate-documents.sh   (from anywhere)
set -euo pipefail
shopt -s nullglob
cd "$(dirname "$0")/.."

# Every spoke validated, and the einvoice-validate rule set of its documents.
spokes=(
    "ubl-invoice:2.1 cen-ubl"
    "xrechnung-invoice:3.0.2 xrechnung-ubl"
    "peppol-bis-billing:3.0 peppol-bis-billing-ubl"
    "facturx-invoice:1.0 factur-x-en16931"
)

if ! command -v einvoice-validate >/dev/null; then
    echo "einvoice-validate is not installed: cargo install --locked einvoice-cli --version 0.4.0" >&2
    exit 1
fi

cargo build --locked -q -p einvoice-interfaces --bin krab-cli
cli="${CARGO_TARGET_DIR:-target}/debug/krab-cli"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

sources=(testfiles/*.xml)
if [ -d testfiles/corpus ]; then
    while IFS= read -r file; do sources+=("$file"); done \
        < <(find testfiles/corpus -name '*.xml' | sort)
fi

# The sources einvoice-validate can check (by their root element: CEN UBL or
# CII) must be valid; FatturaPA sources pass unchecked.
checked=()
valid=()
for src in "${sources[@]}"; do
    if grep -q 'FatturaElettronica[[:space:]>]' "$src"; then
        valid+=("$src")
    else
        checked+=("$src")
    fi
done
while IFS= read -r src; do valid+=("$src"); done \
    < <(einvoice-validate "${checked[@]}" | sed -n 's/^\([^ ].*\): valid.*$/\1/p' || true)
echo "${#sources[@]} sources, $((${#sources[@]} - ${#valid[@]})) skipped as invalid themselves"

status=0
for spoke in "${spokes[@]}"; do
    target="${spoke% *}"
    rules="${spoke#* }"
    mkdir "$work/$target"
    skipped=0
    for src in "${valid[@]}"; do
        rc=0
        "$cli" "$src" "$target" --out "$work/$target/$(echo "$src" | tr / _)" 2>"$work/err" || rc=$?
        case "$rc" in
            0) ;;
            # No spoke reads the source (64), or the spoke refuses it (65).
            64 | 65) skipped=$((skipped + 1)) ;;
            *) echo "$src -> $target: krab-cli exited with $rc" >&2; cat "$work/err" >&2; status=1 ;;
        esac
    done
    written=("$work/$target"/*.xml)
    echo "== $target ($rules): ${#written[@]} written, $skipped not read or refused"
    [ ${#written[@]} -gt 0 ] || continue
    # Lists the invalid documents with their findings, and the totals.
    einvoice-validate --rules "$rules" --jobs 4 "${written[@]}" \
        | sed "s|$work/$target/||" | awk '/^[^ ]/ { show = ($0 !~ /: valid/) } show' || status=1
done
exit "$status"
