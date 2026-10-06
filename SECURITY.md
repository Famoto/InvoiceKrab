# Security policy

## Supported versions

Security fixes are made on the latest 1.x release. Build from the newest tag
(see [CHANGELOG.md](CHANGELOG.md)).

## Reporting a vulnerability

Please do **not** open a public issue for a vulnerability. Report it
privately through GitHub's
[private vulnerability reporting](https://github.com/Famoto/InvoiceKrab/security/advisories/new)
with a description, the affected version (`krab-cli --version`) and, if you
can, a document or request that reproduces it. Never include real invoices:
they carry personal and commercial data.

You will get an answer within a few working days; fixes are released as a
patch version and credited in the changelog unless you prefer otherwise.

## Scope

In scope: the engine, the mapping compiler and the code it generates, the
`krab-cli` and `krab-server` programs, and the Dockerfile. For example: a
document or request that crashes or hangs the server, exceeds its memory
budget, or reads files it should not.

Out of scope: the correctness of the bundled demo mappings against the
invoice standards (see the README's disclaimer), and deployments that expose
`krab-server` without the authenticating reverse proxy it requires (it has no
authentication or TLS of its own).
