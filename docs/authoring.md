# Author a reusable module

This walkthrough reads one small reusable module, selects it in two profiles and checks the result with `explain`. The module exposes one bounded preference (`policy.defaults`) and one contribution map (`policy.extensions`). It also shows the real error text for three common mistakes.

Every command runs against a copy of the synthetic fixtures in `examples/` and an isolated HOME. No command reads or writes your HOME. The test `crates/cli/tests/authoring.rs` runs each command in the `console` blocks, in order, and compares the output with the text shown. It sets `DFM` itself and does not run the `sh` blocks. Run it with:

```sh
cargo test --locked -p dfm-cli --features test-isolation --test authoring
```

The fixtures are in `examples/authoring/`. For the bundle layout and the source roots, see [docs/first-run.md](first-run.md), section 2. For the resolver rules, see [docs/resolver.md](resolver.md).

## 1. Prepare an isolated workspace

The flag `--home` exists only in a build with the feature `test-isolation`. Run the commands from the repository root.

1. Build the isolated binary and point `DFM` at it:

   ```sh
   cargo build --locked -p dfm-cli --features test-isolation
   export DFM=$PWD/target/debug/dfm
   ```

2. Copy the fixtures into a scratch directory, `$W`. The mistakes change files, and the copy keeps the checked-in fixtures unchanged:

   ```console
   $ W=$(mktemp -d "${TMPDIR:-/tmp}/dfm-authoring.XXXXXX")
   $ cp -R examples "$W/examples"
   $ dfm() { "$DFM" --home "$H" "$@"; }
   ```

   The function `dfm` runs the isolated binary with the HOME in `$H`. Each profile gets its own `$H` before it is bound.

## 2. Read the reusable module

The module `git-extras` declares one resource, `config`. It renders a Git config fragment at `.config/git/extras.inc`.

```console
$ cat "$W/examples/authoring/modules/git-extras/module.toml"
schema = 1
id = "git-extras"
platforms = ["macos", "linux"]
requires = []

[[resources]]
id = "config"
kind = "render"
adapter = "git-config/v1"
target = ".config/git/extras.inc"
[resources.value.pull]
rebase = true
[resources.value.rerere]
enabled = true
[resources.value.diff]
[resources.policy]
defaults = ["/pull/rebase"]
extensions = ["/diff"]
```

- `policy.defaults` lists the paths that another module or a profile may override. Here only `/pull/rebase` is exposed. An override must keep the type of the owner's value (a Boolean here), so the preference is bounded.
- `/rerere/enabled` is not listed. Nobody but the owner can change it.
- `policy.extensions` lists the maps that other modules may add keys to. Here `/diff` is an empty map of Git diff drivers. A contributed key must start with the contributor's module ID and a dot.
- The module lists both `macos` and `linux` in `platforms`, so any profile can select it.

## 3. Add the module to a bundle

The bundle `example-authoring` extends the base bundle. It adds `git-extras` and a second module, `macos-tools`. Each catalogue key must equal the module ID. The core comes from the base bundle.

```console
$ cat "$W/examples/authoring/bundle.toml"
# Synthetic bundle for docs/authoring.md. It adds two modules to the base bundle.
schema = 1
id = "example-authoring"
extends = "../base/bundle.toml"

[modules]
git-extras = "modules/git-extras/module.toml"
macos-tools = "modules/macos-tools/module.toml"
```

## 4. Contribute from a second module

The module `macos-tools` adds one diff driver to the exposed map. It requires `git-extras`, so a profile that selects `macos-tools` gets `git-extras` too.

```console
$ cat "$W/examples/authoring/modules/macos-tools/module.toml"
schema = 1
id = "macos-tools"
platforms = ["macos"]
requires = ["git-extras"]

[[contributions]]
resource = "git-extras.config"
path = "/diff"
key = "macos-tools.plist"
reason = "Show binary property lists as XML in git diff."
[contributions.value]
textconv = "plutil -convert xml1 -o -"
```

A contribution names the owner's resource, the exposed map, a new key and a reason. A contribution cannot replace a key that the owner already declares.

## 5. Select the module in the first profile

The profile `laptop-01` selects both modules and overrides nothing.

```console
$ cat "$W/examples/authoring/profiles/laptop-01.toml"
schema = 1
id = "laptop-01"
bundle = "../bundle.toml"
platform = "macos"
architecture = "arm64"
modules = ["git-extras", "macos-tools"]
overrides = []
```

1. Make an isolated HOME for this profile:

   ```console
   $ H=$W/laptop
   $ mkdir "$H"
   ```

2. Bind the profile. `--root` is the copied `examples` directory, because the bundle extends `../base/bundle.toml`:

   ```console
   $ dfm bind --profile "$W/examples/authoring/profiles/laptop-01.toml" --machine-id laptop-01 --root "$W/examples"
   isolated: true home=$W/laptop binding=$W/laptop/.config/dotfiles-manager/machine.toml
   bound $W/examples/authoring/profiles/laptop-01.toml as laptop-01 with root $W/examples
   ```

3. Explain the resource:

   ```console
   $ dfm explain git-extras.config
   isolated: true home=$W/laptop binding=$W/laptop/.config/dotfiles-manager/machine.toml
   profile laptop-01 (macos/arm64), modules: core, git-extras, macos-tools
   resource git-extras.config
     declared by module git-extras in authoring/modules/git-extras/module.toml
     kind render, adapter git-config/v1
     target .config/git/extras.inc
     permitted overrides: /pull/rebase
     permitted extensions: /diff
     native precedence: this adapter writes into a native document. Rule: later include entries override earlier ones for single-valued keys. The manager does not validate the resulting native configuration, so it cannot enforce a protected setting there. Keys that the authored document sets itself are not modelled.
     runtime verification: not verified. The resolver reads declarations only and inspects no machine state.
     values:
       /diff/macos-tools.plist/textconv = "plutil -convert xml1 -o -"
         owner git-extras
         changed by macos-tools (contribution)
         reason: Show binary property lists as XML in git diff.
         supplied by authoring/modules/macos-tools/module.toml
       /pull/rebase = true
         owner git-extras
         declared value, not changed
         supplied by authoring/modules/git-extras/module.toml
       /rerere/enabled = true
         owner git-extras
         declared value, not changed
         supplied by authoring/modules/git-extras/module.toml
   ```

Read the output:

- `declared by module git-extras in ...` is the declared owner of the resource.
- `permitted overrides` and `permitted extensions` repeat the owner's policy.
- For each value, `owner` names the owning module. `changed by` names the module or profile that changed the value, and how. `supplied by` names the manifest that supplied the value. A value that nobody changed shows `declared value, not changed`.
- The contributed diff driver is owned by `git-extras` but supplied by `macos-tools`.

## 6. Select the module in the second profile

The profile `desk-01` selects only `git-extras`. It overrides the exposed preference.

```console
$ cat "$W/examples/authoring/profiles/desk-01.toml"
schema = 1
id = "desk-01"
bundle = "../bundle.toml"
platform = "macos"
architecture = "arm64"
modules = ["git-extras"]

[[overrides]]
resource = "git-extras.config"
path = "/pull/rebase"
value = false
reason = "This machine merges on pull."
```

A binding cannot change its profile (`E_REBIND_UNSUPPORTED`), so this profile gets its own HOME.

1. Make an isolated HOME for this profile:

   ```console
   $ H=$W/desk
   $ mkdir "$H"
   ```

2. Bind the profile:

   ```console
   $ dfm bind --profile "$W/examples/authoring/profiles/desk-01.toml" --machine-id desk-01 --root "$W/examples"
   isolated: true home=$W/desk binding=$W/desk/.config/dotfiles-manager/machine.toml
   bound $W/examples/authoring/profiles/desk-01.toml as desk-01 with root $W/examples
   ```

3. Explain the resource:

   ```console
   $ dfm explain git-extras.config
   isolated: true home=$W/desk binding=$W/desk/.config/dotfiles-manager/machine.toml
   profile desk-01 (macos/arm64), modules: core, git-extras
   resource git-extras.config
     declared by module git-extras in authoring/modules/git-extras/module.toml
     kind render, adapter git-config/v1
     target .config/git/extras.inc
     permitted overrides: /pull/rebase
     permitted extensions: /diff
     native precedence: this adapter writes into a native document. Rule: later include entries override earlier ones for single-valued keys. The manager does not validate the resulting native configuration, so it cannot enforce a protected setting there. Keys that the authored document sets itself are not modelled.
     runtime verification: not verified. The resolver reads declarations only and inspects no machine state.
     values:
       /diff = {}
         owner git-extras
         declared value, not changed
         supplied by authoring/modules/git-extras/module.toml
       /pull/rebase = false
         owner git-extras
         changed by desk-01 (override)
         reason: This machine merges on pull.
         declared value: true
         supplied by authoring/profiles/desk-01.toml
       /rerere/enabled = true
         owner git-extras
         declared value, not changed
         supplied by authoring/modules/git-extras/module.toml
   ```

The value `/pull/rebase` is now `false`. `changed by desk-01 (override)`, the reason and `supplied by authoring/profiles/desk-01.toml` show where the change comes from. `declared value: true` shows the owner's value. The map `/diff` is empty, because this profile does not select `macos-tools`.

## 7. Recognize three common mistakes

Each mistake changes one file in `$W`, runs `explain`, and then restores the file from the repository. Every resolver error stops the command with status 3, before any change to HOME. The desk binding is still active.

### A resource source outside the module root

A resource source must stay inside the directory of the module that declares it.

1. Add a link resource whose source leaves the module directory:

   ```console
   $ cat >> "$W/examples/authoring/modules/git-extras/module.toml" <<'EOF'
   > [[resources]]
   > id = "attributes"
   > kind = "link"
   > adapter = "fs-link/v1"
   > target = ".config/git/attributes"
   > [resources.value]
   > source = "../../../base/modules/core/files/gitconfig"
   > EOF
   ```

2. Explain the resource:

   ```console
   $ dfm explain git-extras.config; echo "status $?"
   isolated: true home=$W/desk binding=$W/desk/.config/dotfiles-manager/machine.toml
   error: E_SOURCE_ESCAPE the path or a symlink in it leaves the trusted root (source: authoring/modules/git-extras/module.toml; resource: git-extras.attributes; field: /source; rejected value: string). Fix: use a path that stays inside the declaring source, and replace or remove any link that leaves it
   status 3
   ```

3. Restore the module:

   ```console
   $ cp examples/authoring/modules/git-extras/module.toml "$W/examples/authoring/modules/git-extras/module.toml"
   ```

To fix it, put the file under the module's own directory, for example `files/attributes`, and use `source = "files/attributes"`.

### An override of a path that the owner does not expose

1. Add an override of `/rerere/enabled` to the desk profile:

   ```console
   $ cat >> "$W/examples/authoring/profiles/desk-01.toml" <<'EOF'
   >
   > [[overrides]]
   > resource = "git-extras.config"
   > path = "/rerere/enabled"
   > value = false
   > reason = "Turn off reuse of recorded resolutions."
   > EOF
   ```

2. Explain the resource:

   ```console
   $ dfm explain git-extras.config; echo "status $?"
   isolated: true home=$W/desk binding=$W/desk/.config/dotfiles-manager/machine.toml
   error: E_OVERRIDE_FORBIDDEN the path is protected: the owner does not list it in policy.defaults (source: authoring/profiles/desk-01.toml; resource: git-extras.config; field: /rerere/enabled; rejected value: boolean; conflicts with: git-extras). Fix: ask the owner of `git-extras.config` to expose the path as a default, or drop the override
   status 3
   ```

3. Restore the profile:

   ```console
   $ cp examples/authoring/profiles/desk-01.toml "$W/examples/authoring/profiles/desk-01.toml"
   ```

To fix it, ask the owner of `git-extras` to add the path to `policy.defaults`, or remove the override.

### A duplicate resource ID

Resource IDs must be unique in one module.

1. Add a second resource with the ID `config`:

   ```console
   $ cat >> "$W/examples/authoring/modules/git-extras/module.toml" <<'EOF'
   > [[resources]]
   > id = "config"
   > kind = "link"
   > adapter = "fs-link/v1"
   > target = ".config/git/attributes"
   > [resources.value]
   > source = "files/attributes"
   > EOF
   ```

2. Explain the resource:

   ```console
   $ dfm explain git-extras.config; echo "status $?"
   isolated: true home=$W/desk binding=$W/desk/.config/dotfiles-manager/machine.toml
   error: E_MANIFEST_INVALID duplicate resource ID in one module (source: authoring/modules/git-extras/module.toml; resource: git-extras.config; field: /resources/1/id). Fix: rename one resource
   status 3
   ```

3. Restore the module:

   ```console
   $ cp examples/authoring/modules/git-extras/module.toml "$W/examples/authoring/modules/git-extras/module.toml"
   ```

To fix it, rename one of the resources.

## 8. Remove the workspace

```console
$ rm -rf "$W"
```
