#!/usr/bin/env bash
# Builds krab-cli against a mapping configuration outside the workspace
# (KRAB_CONFIG_DIR) that compiles two versions of one format, and checks what
# users of their own mappings rely on:
#   - the build compiles exactly the mappings of KRAB_CONFIG_DIR;
#   - several versions of a format coexist, each named by its full name;
#   - a bare format name shared by several versions is refused (exit 64).
#
# Usage: scripts/check-custom-config.sh   (from the repository root)
set -euo pipefail

root="$(pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# A self-contained project: its config/ plus the files its mappings declare
# (declared paths are relative to the config directory's parent).
cp -r "$root/config" "$root/testfiles" "$work/"
rm "$work/config/mappings/fatturapa.toml"
# A second XRechnung version: its own version, profile and model id; it
# declares no samples of its own.
sed -e 's/^format_version = "3.0.2"/format_version = "3.1"/' \
    -e 's/^source_model = "xrechnung-invoice:3.0.2"/source_model = "xrechnung-invoice:3.1"/' \
    -e 's/xrechnung_3\.0/xrechnung_3.1/g' \
    -e '/^\[\[meta.samples\]\]$/,/^file = /d' \
    "$work/config/mappings/xrechnung.toml" > "$work/config/mappings/xrechnung-3.1.toml"

export KRAB_CONFIG_DIR="$work/config"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}/custom-config"
cargo build --locked -q -p einvoice-interfaces --bin krab-cli
cli="$CARGO_TARGET_DIR/debug/krab-cli"

expected="$(printf '    %s\n' facturx-invoice:1.0 peppol-bis-billing:3.0 ubl-invoice:2.1 \
    xrechnung-invoice:3.0.2 xrechnung-invoice:3.1)"
listed="$("$cli" --list)"
if [ "$listed" != "$expected" ]; then
    printf 'unexpected --list:\n%s\nexpected:\n%s\n' "$listed" "$expected" >&2
    exit 1
fi

"$cli" "$root/testfiles/en16931-full-ubl.xml" xrechnung-invoice:3.1 \
    | grep -q 'xrechnung_3.1' || { echo "3.1 was not written as 3.1" >&2; exit 1; }

status=0
"$cli" "$root/testfiles/en16931-full-ubl.xml" xrechnung-invoice >/dev/null 2>&1 || status=$?
if [ "$status" -ne 64 ]; then
    echo "a bare name of two versions must exit 64, got $status" >&2
    exit 1
fi

echo "custom configuration: ok"
