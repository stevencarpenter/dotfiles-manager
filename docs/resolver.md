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

## Rules the architecture leaves open

Each item names the text it extends. None changes a fixture or a documented code.

1. **Module order.** DESIGN.md section 3 says "core is first and remaining ready modules are ordered lexically". The fixtures require `core, server, homelab` for the homelab case, so "ready" means "all prerequisites already placed", with lexical order among ready modules.
2. **Trusted root.** DESIGN.md section 4 says each source "remains within the supplying module's trusted root". The resolver takes that root to be the directory of the manifest that supplies the value: the module for a module value or override, the profile directory for a profile override (an inference, because the text names only modules). A bundle's `core` and catalogue paths stay inside the bundle directory. `extends` and the profile `bundle` may name any bundle in the materialized sources. The `source-containment` case does not separate a module root from a bundle root, because its path leaves both.
3. **`..` and symlinks.** The resolver never collapses `..` before it follows links. It resolves each step against the snapshot, as a filesystem does, and every hop must stay under the root.
4. **Core in selection.** `core` is implicit. Naming it in `modules` or `requires` is `E_MODULE_DUPLICATE`. A core with `requires` is `E_MANIFEST_INVALID`. A catalogue key `core` is `E_CORE_REPLACEMENT`.
5. **Override targets.** A module may override a default of any resource in the selected composition, including its own. Authority comes only from `policy.defaults`, never from `requires`.
6. **Contributions.** Only modules contribute. A profile has no `contributions` key (unknown key). The key must be `<contributor id>.` followed by `[a-z0-9][a-z0-9._-]*`. A contribution to a path not listed in `policy.extensions` is `E_OVERRIDE_FORBIDDEN`.
7. **Output paths.** In addition to DESIGN.md section 4, a target with a control character or a leading `~` component is `E_TARGET_INVALID`. Claims are compared after Unicode lowercasing per component, and a claimed file may not also be the parent of another claim. Unicode normalization forms are not folded.
8. **Distribution claims.** `skills/v1` claims `<root>/<entry>` for each entry whose `enabled` is not `false`. A disabled entry has no claim.
9. **Limits.** Documents: 1 MiB, nesting depth 32, strings 64 KiB, 10,000 resources, bundle ancestry 16 (architecture section 5, S10). The CLI source loader reads at most 20,000 entries, 1 MiB per file and 64 MiB in total, and skips a top-level `.git` directory.
10. **Adapter schemas.** The resolver validates the effective `value` against a schema per adapter. The schemas are the keys the fixtures use. Native documents (`git-config/v1` and the managed and seed parts of `claude-settings/v1`) keep their types without key checks, because their keys belong to the application.
11. **Secret-bearing resources.** `op-template/v1` values are references. `explain` prints `[redacted]` for them, and the resolver records no content identity for them.
12. **`explain` source root.** There is no source lock yet, so `explain` takes `--root <dir>`, which must contain the bound profile. The profile comes from the binding. No `--profile` flag exists.

## Not enforced here

- **Public closure must not reach an external source** (section 6 item 5). Nothing in schema 1 labels a bundle public or external. The resolver enforces the parts it can see: a bundle cannot reach modules outside its own directory, `extends` cannot form a cycle, and the public fixture resolves without any private file. A full check needs source classification from the lock.
- **Native precedence.** `explain` reports that the application decides precedence for native documents and that the manager does not validate the resulting file. The fixtures declare no include order, so no order is shown.
- **Case behavior of the target filesystem.** The resolver folds case for every target. It does not ask the filesystem.
- **Real symlinks at apply time.** The resolver decides containment on a snapshot. The adapter must repeat the check with descriptor-relative operations (architecture section 8, "Filesystem writes").
