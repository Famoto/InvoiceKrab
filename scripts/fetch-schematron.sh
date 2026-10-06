#!/usr/bin/env bash
# Fetches the official business-rule (Schematron) validators the conformance
# checks run, into `target/schematron/` (or the directory given as $1):
#
#   saxon.jar, xmlresolver.jar          Saxon-HE 12.5 (XSLT 3.0) and its resolver
#   en16931/EN16931-{UBL,CII}-validation.xslt   CEN EN 16931 rules 1.3.16
#   xrechnung/XRechnung-{UBL,CII}-validation.xslt   KoSIT XRechnung 3.0.2 rules
#   peppol/{CEN-EN16931-UBL,PEPPOL-EN16931-UBL}.xslt   OpenPeppol BIS Billing 2026.5
#
# The rules ship as compiled XSLT in the phive rule packages (Apache-2.0) on
# Maven Central. Every download is pinned by version and SHA-256, so a changed
# artifact fails the fetch instead of silently changing the rules.
#
# `[meta.schema].schematron` in a mapping names its rule sets relative to this
# directory; `krab-cli --check` and `cargo test` run them when it exists (or
# when KRAB_SCHEMATRON names it) and `java` is on PATH, and skip them with a
# notice otherwise.
set -euo pipefail

dest="${1:-${KRAB_SCHEMATRON:-target/schematron}}"
maven="https://repo1.maven.org/maven2"
mkdir -p "$dest/jars"

# name|maven path|sha256
artifacts=(
  "saxon.jar|net/sf/saxon/Saxon-HE/12.5/Saxon-HE-12.5.jar|98c3a91e6e5aaf9b3e2b37601e04b214a6e67098493cdd8232fcb705fddcb674"
  "xmlresolver.jar|org/xmlresolver/xmlresolver/5.2.2/xmlresolver-5.2.2.jar|efc92bd7ed32b3e57095e0b3e872051ccfbbdcc980831ef33e89e38161a85222"
  "jars/phive-rules-en16931.jar|com/helger/phive/rules/phive-rules-en16931/4.6.3/phive-rules-en16931-4.6.3.jar|9ee9f0e6f90824620a8d6158e3a68e7c07af4072308574bd3be95cd41a21fae4"
  "jars/phive-rules-xrechnung.jar|com/helger/phive/rules/phive-rules-xrechnung/4.6.3/phive-rules-xrechnung-4.6.3.jar|7d62bb3c1cb3af0e9260b2451ac9421d9d8b4625618b302ddf3bf10932343e88"
  "jars/phive-rules-peppol.jar|com/helger/phive/rules/phive-rules-peppol/4.6.3/phive-rules-peppol-4.6.3.jar|8d746eed379b5e9e8cd58530e1b8a7931786d089fb94d785e99b9be142c0431b"
)

verified() { [ -f "$1" ] && echo "$2  $1" | sha256sum --check --status; }

for entry in "${artifacts[@]}"; do
  IFS='|' read -r name path sum <<<"$entry"
  file="$dest/$name"
  if verified "$file" "$sum"; then
    continue
  fi
  # Maven Central rate-limits bursts: retry with backoff.
  for wait in 0 5 15 30 60; do
    sleep "$wait"
    if curl -fsSL --retry 2 -o "$file" "$maven/$path" && verified "$file" "$sum"; then
      break
    fi
    rm -f "$file"
  done
  if ! verified "$file" "$sum"; then
    echo "fetch-schematron: could not fetch a verified $name from $maven/$path" >&2
    exit 1
  fi
done

# rule set|jar|path inside the jar
rules=(
  "en16931/EN16931-UBL-validation.xslt|phive-rules-en16931|external/schematron/1.3.16/ubl/EN16931-UBL-validation.xslt"
  "en16931/EN16931-CII-validation.xslt|phive-rules-en16931|external/schematron/1.3.16/cii/EN16931-CII-validation.xslt"
  "xrechnung/XRechnung-UBL-validation.xslt|phive-rules-xrechnung|external/schematron/3.0.2/XRechnung-UBL-validation.xslt"
  "xrechnung/XRechnung-CII-validation.xslt|phive-rules-xrechnung|external/schematron/3.0.2/XRechnung-CII-validation.xslt"
  "peppol/CEN-EN16931-UBL.xslt|phive-rules-peppol|external/schematron/openpeppol/2026.5/xslt/CEN-EN16931-UBL.xslt"
  "peppol/PEPPOL-EN16931-UBL.xslt|phive-rules-peppol|external/schematron/openpeppol/2026.5/xslt/PEPPOL-EN16931-UBL.xslt"
)
for entry in "${rules[@]}"; do
  IFS='|' read -r target jar inner <<<"$entry"
  mkdir -p "$dest/$(dirname "$target")"
  unzip -p "$dest/jars/$jar.jar" "$inner" >"$dest/$target"
done

echo "fetch-schematron: rules ready in $dest"
