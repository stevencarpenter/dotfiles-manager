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
9. **Limits.** Documents: 1 MiB, nesting depth 32, strings 64 KiB, 10,000 resources, bundle ancestry 16 (architecture section 5, S10). Added by DFM-15 to bound cost: 10,000 modules, 64 MiB of module documents and 200,000 values in one composition, all refused with `E_LIMIT_EXCEEDED` before provenance is built. `explain` clips each text field to 200 characters and refuses a report over 100 resources or 1 MiB. The CLI source loader reads at most 20,000 entries, 1 MiB per file and 64 MiB in total.
10. **Source loading.** The loader opens each file with `O_NOFOLLOW` and `O_NONBLOCK` and confirms a regular file with fstat. It skips a top-level `.git`, which is repository metadata. A `.git` deeper in the tree is a submodule or gitlink, and architecture section 8 rejects those, so it is `E_SOURCE_UNTRUSTED`. A FIFO, device or socket is `E_SOURCE_UNTRUSTED`.
11. **Adapter schemas.** The resolver validates the effective `value` against a schema per adapter. The schemas are the keys the fixtures use. Native documents (`git-config/v1` and the managed and seed parts of `claude-settings/v1`) keep their types without key checks, because their keys belong to the application.
12. **Secret-bearing resources.** `op-template/v1` values are references. `explain` prints `[redacted]` for them and the resolver records no content identity for them. The file mode must be an integer in `0..=0o777` with group and other bits clear.
13. **`explain` source root.** There is no source lock yet, so `explain` takes `--root <dir>`, which must contain the bound profile. The profile comes from the binding. No `--profile` flag exists.
14. **Fragment directories.** `text-fragments/v1` reads fragments from `fragments_dir`; it does not write there. The review asked for the directory to be claimed. A blanket claim would reject the `work` module's fragment `.codex/AGENTS.d/40-work.md`, which `external-composition` requires to resolve, because supplying a fragment from another module is the purpose of the directory (DESIGN.md section 3). The directory is therefore claimed as a prefix with one exception: another resource may place a file directly in it when the file name matches the owner's `pattern`. Anything else collides: a non-matching name, a nested path, the directory itself, a case alias of those, a second assembler whose directory equals or nests with this one, and the assembled file inside its own directory. The pattern allows `*` and `?` only.
15. **Case.** Source lookup matches without regard to case, as a case-insensitive filesystem does. A link spelled `Link` is therefore followed for `link`, and two entries that differ only by case are `E_SOURCE_ESCAPE`. `..` applies only to a directory that exists in the snapshot.
16. **Display.** Control characters, zero-width characters, the byte order mark and bidirectional controls are escaped in every message and `explain` line. A target or source path that contains one is refused.

## Native precedence

DESIGN.md section 5 requires native precedence to be visible per value. The resolver reads it from the authored document, not from a manifest field. An `fs-link/v1` source that is a native config is scanned for include directives (the CLI registers a Git parser). The result is `Resolved.includes`: for each such document, the includes in file order, each with its condition and the resource that declares the included file (matched by output path). `explain` prints, for every native fragment, its position and condition, the ordered siblings, the adapter's ordering rule, and for each value the other fragments that also set it. For a Git config the rule is "later include entries override earlier ones for single-valued keys". Only values set in more than one fragment depend on the order. A native document that no authored file includes reports that the application decides, and no order is invented.

The manager still cannot validate the resulting native file, so a protected setting is not enforceable against a fragment. Adapter-level validation belongs to DFM-20.

## Not enforced here

- **Public closure must not reach an external source** (architecture section 6 item 5). **The resolver deviates from that rule until a source lock exists.** Schema 1 labels no bundle public or external, and the lock that would supply the classification is not implemented. The resolver enforces what it can see: a bundle cannot reach modules outside its own directory, `extends` cannot form a cycle, and the public fixture resolves with no private file present. The lock-based check has no ticket of its own. DFM-30 (source staging, trust and lock) is the nearest owner, and it needs this criterion added. Until then, nothing stops a public bundle from extending a private one.
- **Unicode normalization.** Output paths and source names are compared after lowercasing only. Two names that differ only by normalization form (NFC against NFD) are not detected as collisions, and APFS treats them as one name. Closing this needs a normalization dependency, which the architecture has not accepted.
- **Real symlinks at apply time.** The resolver decides containment on a snapshot. The adapter must repeat the check with descriptor-relative operations (architecture section 8, "Filesystem writes").
- **Exit mapping.** `Code::exit()` maps every resolver code to exit 3, which is right for `explain`, `plan` and mutating commands (a gate failed before mutation). `doctor` must not reuse that mapping: architecture section 10 says `doctor` reports malformed manifests as findings, never exits 3 or 4, and exits 1. DFM-35 must map resolver errors itself.
- **Secret redaction depends on discipline.** Errors never copy a value. `explain` redacts values of `secret_values` resources. Nothing else prints resolved values, but `Debug` on a `ResolvedResource` shows them.
