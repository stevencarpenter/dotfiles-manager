# Resolver (DFM-15)

Status: implemented in `crates/model/src/resolve` (pure, no I/O). The adapter registry that the resolver validates against is built by the CLI (`crates/cli/src/adapters.rs`). [docs/architecture.md](architecture.md) is the binding contract. This page records only what that contract leaves open.

## Validation order

The first error follows DESIGN.md section 9.

1. Document and bundle schema: profile, bundle chain, core, selected modules and their prerequisites.
2. Selection and dependency validity: unknown, duplicate, platform, cycle.
3. Composition authority and conflicts: overrides and contributions.
4. Resolved adapter schema and source containment.
5. Output claims.

## Error codes added by the resolver

The `expected_error` codes in `examples/cases.toml` and `E_ADAPTER_UNKNOWN` are fixed by the docs. The architecture does not name codes for the cases below, so these are new.

| Code | Meaning |
| --- | --- |
| `E_FIELD_UNKNOWN` | A key that schema 1 does not define (document or adapter value). |
| `E_MANIFEST_INVALID` | Missing or wrongly typed key, invalid ID, invalid policy path, a core that requires modules, duplicate bundle or resource ID. |
| `E_REFERENCE_INVALID` | A reference to the wrong kind of document, an unknown or unselected resource, a catalogue key that does not match the module ID, an invalid override path. |
| `E_TYPE_MISMATCH` | A value whose type differs from the owner's value or the adapter schema. Boolean and integer are different types. |
| `E_EXTENSION_CONFLICT` | A contribution to a key that already exists, or two contributors for one key. |
| `E_BUNDLE_CYCLE` | Bundles extend each other in a cycle. |

All resolver codes exit 3 (blocked before mutation).
