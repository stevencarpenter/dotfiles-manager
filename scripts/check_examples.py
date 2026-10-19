#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.14"
# dependencies = []
# ///
"""Check design fixtures without resolving compositions or operating on HOME."""

from __future__ import annotations

import re
import shutil
import tempfile
import tomllib
from pathlib import Path
from typing import Any

ADAPTERS = {
    "link": {"fs-link/v1"},
    "render": {"git-config/v1"},
    "merge": {"claude-settings/v1"},
    "assemble": {"text-fragments/v1"},
    "distribution": {"skills/v1"},
    "secret": {"op-template/v1"},
    "package": {"homebrew/v1", "mise/v1", "apt/v1", "uv-tool/v1"},
    "application": {"firstmate-native/v1"},
}
FILE_KINDS = {"link", "render", "merge", "assemble", "secret"}


def require(condition: bool, message: str) -> None:
    """Reject a failed fixture assertion.

    Args:
        condition (bool): Whether the assertion holds.
        message (str): Diagnostic for a failed assertion.

    Raises:
        ValueError: If the assertion fails.
    """
    if not condition:
        raise ValueError(message)


def shape(value: Any, required: set[str], optional: set[str], label: str) -> None:
    """Check an example table's required and permitted keys.

    Args:
        value (Any): Parsed TOML table.
        required (set[str]): Keys that must exist.
        optional (set[str]): Additional permitted keys.
        label (str): Diagnostic context.

    Raises:
        ValueError: If the table shape is invalid.
    """
    require(isinstance(value, dict), f"{label}: expected a table")
    require(required <= value.keys(), f"{label}: missing {required - value.keys()}")
    require(value.keys() <= required | optional, f"{label}: unknown keys")


def local_path(root: Path, candidate: Path) -> Path:
    """Require an existing fixture path contained in the declared root.

    Args:
        root (Path): Allowed filesystem root.
        candidate (Path): Proposed file or directory.

    Returns:
        Path: Canonical contained path.

    Raises:
        ValueError: If the path escapes the root or does not exist.
    """
    resolved = candidate.resolve()
    require(
        resolved.is_relative_to(root.resolve()), f"source escapes root: {candidate}"
    )
    require(resolved.exists(), f"missing fixture reference: {candidate}")
    return resolved


def home_path(value: Any) -> None:
    """Validate a HOME-relative example target without accessing HOME.

    Args:
        value (Any): Proposed relative target string.

    Raises:
        ValueError: If the path is unsafe or not a string.
    """
    require(isinstance(value, str) and bool(value), "target must be a nonempty string")
    require("\0" not in value, f"NUL in target: {value!r}")
    require(
        all(part not in {"", ".", ".."} for part in value.split("/")),
        f"unsafe target: {value}",
    )


def pointer(value: Any, path: str) -> Any:
    """Read an object-only policy pointer, not a JSON Patch array address.

    Args:
        value (Any): Resource value tree.
        path (str): Non-root RFC 6901 pointer.

    Returns:
        Any: Referenced value.

    Raises:
        ValueError: If the pointer is malformed, missing, or traverses an array.
    """
    require(isinstance(path, str) and path.startswith("/"), f"invalid pointer: {path}")
    require(re.search(r"~(?![01])", path) is None, f"invalid escape: {path}")
    for token in path[1:].split("/"):
        token = token.replace("~1", "/").replace("~0", "~")
        require(
            token != "*" and isinstance(value, dict) and token in value,
            f"unknown policy path: {path}",
        )
        value = value[token]
    return value


def check(root: Path) -> tuple[int, int, int]:
    """Validate draft syntax and references, not composition outcomes.

    Args:
        root (Path): Isolated or checked-in examples directory.

    Returns:
        tuple[int, int, int]: Manifest, module, and acceptance-case counts.

    Raises:
        ValueError: If an example violates the checked structural contracts.
        tomllib.TOMLDecodeError: If a TOML document does not parse.
    """
    docs: dict[Path, dict[str, Any]] = {}
    for path in sorted(root.rglob("*.toml")):
        data = tomllib.loads(path.read_text())
        if "files" in path.relative_to(root).parts:
            continue  # Native TOML is syntax-checked but is not a manager manifest.
        require(
            type(data.get("schema")) is int and data["schema"] == 1,
            f"{path}: schema must be 1",
        )
        docs[path.resolve()] = data
    modules = {path: doc for path, doc in docs.items() if path.name == "module.toml"}
    ids = [doc["id"] for doc in modules.values()]
    require(
        len(ids) == len(set(ids)) and ids.count("core") == 1,
        "duplicate module ID or missing core",
    )
    resources: dict[str, dict[str, Any]] = {}
    for path, doc in modules.items():
        shape(
            doc,
            {"schema", "id", "platforms", "requires"},
            {"resources", "overrides", "contributions"},
            str(path),
        )
        require(
            re.fullmatch(r"[a-z][a-z0-9]*(?:-[a-z0-9]+)*", doc["id"]) is not None,
            f"{path}: invalid ID",
        )
        require(
            bool(doc["platforms"]) and set(doc["platforms"]) <= {"macos", "linux"},
            f"{path}: invalid platforms",
        )
        require(set(doc["requires"]) <= set(ids), f"{path}: unknown dependency")
        for resource in doc.get("resources", []):
            shape(
                resource,
                {"id", "kind", "adapter", "value"},
                {"target", "policy"},
                str(path),
            )
            name = f"{doc['id']}.{resource['id']}"
            require(name not in resources, f"duplicate resource: {name}")
            resources[name] = resource
            require(
                resource["adapter"] in ADAPTERS.get(resource["kind"], set()),
                f"{name}: unknown adapter contract",
            )
            require(
                isinstance(resource["value"], dict), f"{name}: value must be a table"
            )
            if resource["kind"] in FILE_KINDS:
                home_path(resource.get("target"))
            else:
                require("target" not in resource, f"{name}: adapter enumerates claims")
            value = resource["value"]
            for key in ("source", "body", "template"):
                if key in value:
                    local_path(path.parent, path.parent / value[key])
            for target in value.get("target_roots", []):
                home_path(target)
            for entry in value.get("entries", {}).values():
                local_path(path.parent, path.parent / entry["source"])
            policy = resource.get("policy", {})
            shape(policy, set(), {"defaults", "extensions"}, name)
            exposed = policy.get("defaults", []) + policy.get("extensions", [])
            require(len(exposed) == len(set(exposed)), f"{name}: duplicate policy")
            for exposed_path in exposed:
                pointer(value, exposed_path)
                require(
                    not any(other.startswith(exposed_path + "/") for other in exposed),
                    f"{name}: overlapping policy",
                )
            for extension in policy.get("extensions", []):
                require(
                    isinstance(pointer(value, extension), dict),
                    f"{name}: extension must be a map",
                )

    bundles = {path for path in docs if path.name == "bundle.toml"}
    profiles = {
        path: doc for path, doc in docs.items() if path.parent.name == "profiles"
    }
    for path, doc in docs.items():
        if path.name == "bundle.toml":
            shape(doc, {"schema", "id", "modules"}, {"core", "extends"}, str(path))
            require(
                ("core" in doc) != ("extends" in doc),
                f"{path}: core xor extends required",
            )
            for key in ("core", "extends"):
                if key in doc:
                    target = local_path(root, path.parent / doc[key])
                    require(
                        target in modules and modules[target]["id"] == "core"
                        if key == "core"
                        else target in bundles,
                        f"{path}: {key} must reference a parsed "
                        + ("core module" if key == "core" else "bundle"),
                    )
            for name, reference in doc["modules"].items():
                target = local_path(root, path.parent / reference)
                require(
                    target in modules and modules[target]["id"] == name,
                    f"{path}: catalogue mismatch",
                )
        elif path in profiles:
            shape(
                doc,
                {
                    "schema",
                    "id",
                    "bundle",
                    "platform",
                    "architecture",
                    "modules",
                },
                {"overrides"},
                str(path),
            )
            require(
                local_path(root, path.parent / doc["bundle"]) in bundles,
                f"{path}: bundle must reference a parsed bundle",
            )
            require(set(doc["modules"]) <= set(ids), f"{path}: unknown selected module")
        if path in modules or path in profiles:
            for override in doc.get("overrides", []):
                shape(
                    override, {"resource", "path", "value", "reason"}, set(), str(path)
                )
                require(
                    override["resource"] in resources
                    and bool(override["reason"].strip()),
                    f"{path}: invalid override reference/reason",
                )
                resource = resources[override["resource"]]
                pointer(resource["value"], override["path"])
                if resource["kind"] == "link" and override["path"] == "/source":
                    local_path(path.parent, path.parent / override["value"])
            for entry in doc.get("contributions", []):
                shape(
                    entry,
                    {"resource", "path", "key", "value", "reason"},
                    set(),
                    str(path),
                )
                require(
                    entry["resource"] in resources,
                    f"{path}: unknown contribution resource",
                )
                require(
                    isinstance(
                        pointer(resources[entry["resource"]]["value"], entry["path"]),
                        dict,
                    ),
                    f"{path}: contribution needs map",
                )
                require(
                    entry["key"].startswith(doc["id"] + ".")
                    and bool(entry["reason"].strip()),
                    f"{path}: invalid contribution key/reason",
                )

    case_doc = docs[(root / "cases.toml").resolve()]
    shape(case_doc, {"schema", "cases"}, set(), "cases.toml")
    case_ids = [case["id"] for case in case_doc["cases"]]
    require(len(case_ids) == len(set(case_ids)), "duplicate acceptance case")
    for case in case_doc["cases"]:
        shape(
            case, {"id", "profile"}, {"expect", "expected_error", "changes"}, case["id"]
        )
        require(
            ("expect" in case) != ("expected_error" in case),
            f"{case['id']}: one expected outcome required",
        )
        require(
            local_path(root, root / case["profile"]) in profiles,
            f"{case['id']}: unknown profile",
        )
        for change in case.get("changes", []):
            shape(change, {"file", "op", "path"}, {"value"}, case["id"])
            require(
                local_path(root, root / change["file"]) in docs,
                f"{case['id']}: unknown patch document",
            )
            require(
                change["op"] in {"add", "replace", "remove"}
                and change["path"].startswith("/"),
                f"{case['id']}: invalid fixture patch",
            )
        for field in case.get("expect", {}).get("values", []):
            pointer(resources[field["resource"]]["value"], field["path"])
        for origin in case.get("expect", {}).get("origins", []):
            local_path(root, root / origin["source"])
    return len(docs), len(modules), len(case_ids)


def main() -> None:
    """Check examples, nine malformed copies, and one valid optional-field omission.

    Returns:
        None: Prints validation evidence or raises on failure.
    """
    root = Path(__file__).resolve().parents[1] / "examples"
    counts = check(root)
    mutations = [
        ("base/bundle.toml", "schema = 1", "schema = 0", "schema must be 1"),
        (
            "base/modules/core/module.toml",
            'target = ".ssh/config"',
            'target = "../unsafe"',
            "unsafe target",
        ),
        (
            "base/modules/core/module.toml",
            'source = "files/ssh_config"',
            'source = "../../../profiles/server-01.toml"',
            "source escapes root",
        ),
        (
            "profiles/personal-dev-01.toml",
            'modules = ["personal-dev"]',
            'modules = ["missing"]',
            "unknown selected module",
        ),
        (
            "base/modules/core/module.toml",
            '"/core/editor",',
            '"/core/missing",',
            "unknown policy path",
        ),
        (
            "base/bundle.toml",
            'core = "modules/core/module.toml"',
            'core = "../external/bundle.toml"',
            "core must reference a parsed core module",
        ),
        (
            "base/bundle.toml",
            'core = "modules/core/module.toml"',
            'core = "modules/server/module.toml"',
            "core must reference a parsed core module",
        ),
        (
            "external/bundle.toml",
            'extends = "../base/bundle.toml"',
            'extends = "../base/modules/core/module.toml"',
            "extends must reference a parsed bundle",
        ),
        (
            "profiles/server-01.toml",
            'bundle = "../base/bundle.toml"',
            'bundle = "../base/modules/server/module.toml"',
            "bundle must reference a parsed bundle",
        ),
        ("profiles/server-01.toml", "overrides = []\n", "", None),
    ]
    for relative, old, new, diagnostic in mutations:
        with tempfile.TemporaryDirectory(prefix="dotfiles-contract-") as directory:
            fixture = Path(directory) / "examples"
            shutil.copytree(root, fixture)
            target = fixture / relative
            original = target.read_text()
            require(
                original.count(old) == 1,
                f"self-check mutation is ambiguous: {relative}",
            )
            target.write_text(original.replace(old, new))
            try:
                result = check(fixture)
            except ValueError as error:
                require(
                    diagnostic is not None and diagnostic in str(error),
                    f"wrong rejection: {error}",
                )
            else:
                require(
                    diagnostic is None,
                    f"checker accepted malformed fixture: {relative}",
                )
                require(result == counts, "valid fixture changed document counts")
    print(
        f"Checked {counts[0]} manifests, {counts[1]} modules, {counts[2]} acceptance-case definitions."
    )
    print(
        "Rejected nine malformed fixture variants; accepted one profile without overrides. "
        "This script checks shape only; the resolver cases run in "
        "crates/cli/tests/resolver (cargo test, step rust-tests)."
    )


if __name__ == "__main__":
    main()
