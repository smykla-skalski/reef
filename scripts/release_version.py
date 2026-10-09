#!/usr/bin/env python3
"""Plan or apply a Reef release version without extra dependencies."""

import argparse
from pathlib import Path
import re
import tomllib


def current_version(root: Path) -> tuple[int, int, int]:
    cargo = tomllib.loads((root / "Cargo.toml").read_text())
    version = cargo["package"]["version"]
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", version)
    if match is None:
        raise ValueError(f"unsupported Cargo package version: {version}")
    return tuple(map(int, match.groups()))


def next_version(current: tuple[int, int, int], bump: str) -> str:
    major, minor, patch = current
    if bump == "major":
        return f"{major + 1}.0.0"
    if bump == "minor":
        return f"{major}.{minor + 1}.0"
    if bump == "patch":
        return f"{major}.{minor}.{patch + 1}"
    raise ValueError(f"unsupported bump: {bump}")


def apply_version(root: Path, version: str) -> None:
    old = ".".join(map(str, current_version(root)))
    if old == version:
        raise ValueError("new version must differ from current version")
    cargo_path = root / "Cargo.toml"
    cargo = cargo_path.read_text()
    package, separator, rest = cargo.partition("[package]\n")
    if not separator:
        raise ValueError("Cargo.toml has no package section")
    package_section, boundary, remainder = rest.partition("\n[")
    updated, count = re.subn(
        rf'(?m)^version = "{re.escape(old)}"$',
        f'version = "{version}"',
        package_section,
    )
    if count != 1:
        raise ValueError("expected one package version in Cargo.toml")
    lock_path = root / "Cargo.lock"
    lock = lock_path.read_text()
    pattern = re.compile(
        rf'(?m)^(\[\[package\]\]\nname = "reef"\nversion = "){re.escape(old)}("$)'
    )
    updated_lock, lock_count = pattern.subn(rf"\g<1>{version}\g<2>", lock)
    if lock_count != 1:
        raise ValueError("expected one Reef package version in Cargo.lock")
    cargo_path.write_text(package + separator + updated + boundary + remainder)
    lock_path.write_text(updated_lock)
    (root / ".release-version").write_text(f"{version}\n")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("current", "next", "apply"))
    parser.add_argument("value", nargs="?")
    parser.add_argument("--root", type=Path, default=Path.cwd())
    args = parser.parse_args()
    current = current_version(args.root)
    if args.command == "current":
        print(".".join(map(str, current)))
    elif args.command == "next":
        if args.value is None:
            parser.error("next requires a bump type")
        print(next_version(current, args.value))
    else:
        if args.value is None or re.fullmatch(r"\d+\.\d+\.\d+", args.value) is None:
            parser.error("apply requires a release version")
        apply_version(args.root, args.value)


if __name__ == "__main__":
    main()
