# einvoice-transformator

## Purpose

`einvoice-transformator` is the small **pure-helper runtime** that the generated
spoke mappers link against. The engine is codegen'd to native Rust at build time
(no VM, no interpreter), and the canonical hub is a **generated typed struct**
(`MainKey`) in the interfaces crate. Generated code uses native Rust types
(`String`, `rust_decimal::Decimal`, `bool`, `Vec<…>`) directly, so this crate owns
only the small set of pure helpers generated code still calls. It has no `Value`
type and no dynamic hub.

It performs no I/O and has **no dependency on `einvoice-dsl`**. Generated code
references the exact type and function names exported here.

## Structure

- `result.rs` — `MappingResult<T>` and `MappingDiagnostic` (+ `Severity`): the
  structured output of a mapper run.
- `normalize.rs` — pure string transforms (`trim`, `uppercase`, `lowercase`,
  `empty_as_missing`) emitted as direct function calls.
- `validate.rs` — lexical shape checks (`is_currency`, `is_date`, `is_datetime`,
  `is_unit_code`) the generated reader calls before building a field.
- `adapter.rs` — the string-based `Adapter` contract (`&str -> Result<String, _>`)
  for named, deterministic, pure conversions invoked by generated code.

## Behavior

Everything here is pure data and pure functions. Generated code normalizes and
validates `&str` inputs, optionally calls adapter functions, and returns a
`MappingResult` carrying diagnostics. Mapping-level problems (missing required
fields, type errors) are *diagnostics*, not Rust errors.

## Testing

Unit tests live beside each module, with doc tests on the public API. There is no
runtime interpreter to property-test; correctness of the generated mappers is
verified end-to-end in `einvoice-interfaces`.
