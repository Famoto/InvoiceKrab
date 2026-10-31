# Test corpus

Third-party test invoices for the transformation correctness harness
(`crates/einvoice-interfaces/tests/transformation_correctness.rs`). They are
not part of the repository: their publishers' terms are their own.
`scripts/fetch-corpus.sh` downloads them into this directory from their
upstream repositories, pinned to a commit, and verifies every file against
`SHA256SUMS`. Files are byte-for-byte as published; nothing is edited.

Run the harness over them with:

```sh
scripts/fetch-corpus.sh
KRAB_CORPUS_DIR=testfiles/corpus cargo test -p einvoice-interfaces --test transformation_correctness
```

| Directory | Content | Upstream | Commit | License |
|---|---|---|---|---|
| kosit/ | XRechnung test suite `src/test/`: business cases (standard, extension) and technical cases (CIUS, CVD), UBL and CII | [itplr-kosit/xrechnung-testsuite](https://github.com/itplr-kosit/xrechnung-testsuite), tag `v2026-08-31` | 44c3905c69ca236aa3b94c04be1b7e9bd066ba03 | Apache-2.0 |
| peppol/ | Peppol BIS Billing 3.0 `rules/examples/` and `rules/national-examples/` | [OpenPEPPOL/peppol-bis-invoice-3](https://github.com/OpenPEPPOL/peppol-bis-invoice-3), tag `3.0.21` | 5df0eff7c8efafd7c79c0dacce55082d175db759 | none declared |

To move to a newer upstream version, change the commit in
`scripts/fetch-corpus.sh`, regenerate `SHA256SUMS` from a checkout of that
commit, and update this table.
