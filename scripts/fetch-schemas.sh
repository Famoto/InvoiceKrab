#!/bin/sh
# Fetches the third-party XSD schemas the bundled mappings declare in
# [meta.schema] into testfiles/xsd/. They are not part of the repository:
# their publishers' terms are their own (see testfiles/xsd/SOURCES.md). The
# build does not need them; the schema checks (tests/xsd_validation.rs,
# krab-cli --check) skip schema validation with a notice until they are here.
#
# Every file is downloaded from a mirror pinned to a commit and verified
# against testfiles/xsd/SHA256SUMS; a file already present with the right
# checksum is kept. Safe to re-run.
#
# Usage: scripts/fetch-schemas.sh   (from anywhere)
set -eu

dir="$(cd "$(dirname "$0")/.." && pwd)/testfiles/xsd"
facturx='https://raw.githubusercontent.com/akretion/factur-x/6ce07409b918bb08766cf90a80546685e218d733/src/facturx/xsd_and_schematron'
oca='https://raw.githubusercontent.com/OCA/l10n-italy/4e06dda6a78efa8e36e28bbdbf95cdc00dd62838/l10n_it_account/tools/xsd'

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

url_of() {
    case "$1" in
        ubl-2.1/* | facturx-en16931/*) echo "$facturx/$1" ;;
        fatturapa-1.2.2/*) echo "$oca/${1#fatturapa-1.2.2/}" ;;
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

echo "schemas: $fetched fetched, all verified in $dir"
