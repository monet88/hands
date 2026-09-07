#!/usr/bin/env python3
"""
Prepare and patch the upstream grok-build checkout for Hands.

Verifies the grok-build checkout against the pinned base commit, checks and
applies the Hands patch set under patches/grok-build/ deterministically,
and fails closed on mismatch or unexpected modifications.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

PINNED_GROK_BUILD_SHA = "72a61251fcffb464bcc687aeb5a998e5a98ec0c9"


def run_git(args: list[str], cwd: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git"] + args,
        cwd=str(cwd),
        capture_output=True,
        text=True,
        check=False,
    )


def get_patch_target(patch_content: str) -> str:
    import re
    m = re.search(r"^diff --git a/(\S+) b/(\S+)", patch_content, re.MULTILINE)
    if not m:
        raise RuntimeError("could not determine patch target from diff header")
    return m.group(1)


def validate_cargo_toml_state(grok_build_path: Path) -> None:
    res = run_git(["show", "HEAD:Cargo.toml"], cwd=grok_build_path)
    if res.returncode != 0:
        raise RuntimeError("failed to read HEAD:Cargo.toml")
    head_toml = res.stdout

    member = '    "crates/codegen/hands",'
    old_member = '    "crates/codegen/grok-harness",'
    needle = '    "crates/codegen/xai-grok-tools",'

    if old_member in head_toml:
        expected = head_toml.replace(old_member, member, 1)
    elif needle in head_toml:
        expected = head_toml.replace(needle, needle + "\n" + member, 1)
    else:
        raise RuntimeError("HEAD:Cargo.toml missing expected anchor for hands injection")

    cargo_toml_file = grok_build_path / "Cargo.toml"
    if not cargo_toml_file.is_file():
        raise RuntimeError(f"Cargo.toml missing in {grok_build_path}")
    actual = cargo_toml_file.read_text(encoding="utf-8")
    if actual.replace("\r\n", "\n") != expected.replace("\r\n", "\n"):
        raise RuntimeError(
            "Cargo.toml does not match the exact deterministic injection transformation from pinned HEAD"
        )

HANDS_CARGO_LOCK_PACKAGE = """[[package]]
name = "hands"
version = "0.1.0"
dependencies = [
 "dirs 5.0.1",
 "dunce",
 "serde_json",
 "serial_test",
 "sha2 0.10.9",
 "similar",
 "tempfile",
 "tokio",
 "xai-grok-tools",
 "xai-tool-types",
]

"""


def validate_cargo_lock_state(grok_build_path: Path) -> None:
    res = run_git(["show", "HEAD:Cargo.lock"], cwd=grok_build_path)
    if res.returncode != 0:
        raise RuntimeError("failed to read HEAD:Cargo.lock")
    head_lock = res.stdout.replace("\r\n", "\n")

    cargo_lock_file = grok_build_path / "Cargo.lock"
    if not cargo_lock_file.is_file():
        raise RuntimeError(f"Cargo.lock missing in {grok_build_path}")
    actual = cargo_lock_file.read_text(encoding="utf-8").replace("\r\n", "\n")

    if actual == head_lock:
        return

    needle = '[[package]]\nname = "hash32"'
    if needle in head_lock:
        expected = head_lock.replace(needle, HANDS_CARGO_LOCK_PACKAGE + needle, 1)
        if actual == expected:
            return

    raise RuntimeError(
        "Cargo.lock does not match the exact deterministic injection transformation from pinned HEAD"
    )

def validate_grok_build_status(
    grok_build_path: Path,
    expected_patch_targets: set[str],
    allow_dirty_lockfile: bool = False,
) -> None:
    res = run_git(["status", "--porcelain"], cwd=grok_build_path)
    if res.returncode != 0:
        raise RuntimeError(
            f"failed to check git status in {grok_build_path}: {res.stderr.strip()}"
        )

    for raw_line in res.stdout.splitlines():
        if not raw_line.strip():
            continue
        path_str = raw_line[3:].strip().strip('"').replace("\\", "/")

        if path_str in expected_patch_targets:
            continue

        if path_str == "Cargo.toml":
            validate_cargo_toml_state(grok_build_path)
            continue

        if path_str == "Cargo.lock":
            if not allow_dirty_lockfile:
                raise RuntimeError(
                    f"unexpected modified/untracked Cargo.lock in grok-build checkout: {raw_line.strip()}"
                )
            validate_cargo_lock_state(grok_build_path)
            continue

        if path_str == "crates/codegen/hands" or path_str.startswith("crates/codegen/hands/"):
            continue

        raise RuntimeError(
            f"unexpected modified/untracked file in grok-build checkout: {raw_line.strip()}"
        )


def normalize_diff(text: str) -> str:
    lines = []
    found_diff = False
    for line in text.replace("\r\n", "\n").splitlines(keepends=True):
        if not found_diff:
            if line.startswith("diff --git "):
                found_diff = True
            else:
                continue
        # Skip git index header which contains repository-size-dependent object hash abbreviations
        # (e.g. "index ec4e53c9..527a0d3c 100644" vs "index ec4e53c..527a0d3 100644")
        if line.startswith("index ") and ".." in line:
            continue
        lines.append(line)
    return "".join(lines)


def verify_target_diffs(
    grok_build_path: Path,
    patch_files: list[Path],
) -> None:
    for patch in patch_files:
        patch_text = patch.read_text(encoding="utf-8")
        target = get_patch_target(patch_text)
        res = run_git(
            [
                "-c",
                "diff.noprefix=false",
                "-c",
                "diff.mnemonicprefix=false",
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--",
                target,
            ],
            cwd=grok_build_path,
        )
        if res.returncode != 0:
            raise RuntimeError(f"failed to diff patch target {target}: {res.stderr.strip()}")
        diff_norm = normalize_diff(res.stdout)
        patch_norm = normalize_diff(patch_text)
        if not diff_norm or diff_norm != patch_norm:
            raise RuntimeError(
                f"patch target {target} diverged from versioned patch {patch.name}"
            )

def verify_and_patch(
    grok_build_path: Path,
    patches_dir: Path,
    expected_sha: str = PINNED_GROK_BUILD_SHA,
) -> None:
    if not grok_build_path.is_dir():
        raise RuntimeError(
            f"grok-build checkout directory does not exist: {grok_build_path}"
        )
    if not (grok_build_path / ".git").exists():
        raise RuntimeError(
            f"grok-build target is not a git repository: {grok_build_path}"
        )
    if not patches_dir.is_dir():
        raise RuntimeError(f"patches directory does not exist: {patches_dir}")

    patch_files = sorted(patches_dir.glob("*.patch"))
    if not patch_files:
        raise RuntimeError(f"no patch files found in {patches_dir}")

    # 1. Check current commit SHA
    res = run_git(["rev-parse", "HEAD"], cwd=grok_build_path)
    if res.returncode != 0:
        raise RuntimeError(
            f"failed to read git HEAD in {grok_build_path}: {res.stderr.strip()}"
        )
    current_sha = res.stdout.strip()
    if current_sha != expected_sha:
        raise RuntimeError(
            f"grok-build commit mismatch: expected pinned SHA {expected_sha}, got {current_sha}"
        )

    expected_targets = {get_patch_target(p.read_text(encoding="utf-8")) for p in patch_files}

    # 2. Check if the patch set is already applied cleanly
    already_applied = True
    for patch in patch_files:
        res = run_git(
            ["apply", "--check", "--reverse", str(patch.resolve())],
            cwd=grok_build_path,
        )
        if res.returncode != 0:
            already_applied = False
            break

    if already_applied:
        # Verify that all target files match the versioned patch set bit-for-bit
        verify_target_diffs(grok_build_path, patch_files)
        # Verify that no other unexpected modifications exist
        validate_grok_build_status(grok_build_path, expected_targets, allow_dirty_lockfile=True)
        return

    # 3. If not already applied, the working directory must not have unexpected modifications
    validate_grok_build_status(grok_build_path, expected_patch_targets=set(), allow_dirty_lockfile=False)

    # 4. Check all patches can apply cleanly
    for patch in patch_files:
        res = run_git(
            ["apply", "--check", str(patch.resolve())],
            cwd=grok_build_path,
        )
        if res.returncode != 0:
            raise RuntimeError(
                f"patch pre-check failed for {patch.name}:\n{res.stderr.strip()}"
            )

    # 5. Apply all patches
    for patch in patch_files:
        res = run_git(
            ["apply", str(patch.resolve())],
            cwd=grok_build_path,
        )
        if res.returncode != 0:
            raise RuntimeError(
                f"patch application failed for {patch.name}:\n{res.stderr.strip()}"
            )

    # 6. Post-apply verification: verify bit-for-bit diff and clean status
    verify_target_diffs(grok_build_path, patch_files)
    validate_grok_build_status(grok_build_path, expected_targets, allow_dirty_lockfile=True)

def main() -> int:
    parser = argparse.ArgumentParser(
        description="Prepare and apply patches to grok-build for Hands."
    )
    parser.add_argument(
        "grok_build_dir",
        type=Path,
        help="Path to grok-build checkout directory",
    )
    parser.add_argument(
        "--patches-dir",
        type=Path,
        default=None,
        help="Path to patches directory (defaults to patches/grok-build relative to Hands repo)",
    )
    args = parser.parse_args()

    hands_repo = Path(__file__).resolve().parent.parent
    patches_dir = args.patches_dir or (hands_repo / "patches" / "grok-build")

    try:
        verify_and_patch(
            grok_build_path=args.grok_build_dir.resolve(),
            patches_dir=patches_dir.resolve(),
            expected_sha=PINNED_GROK_BUILD_SHA,
        )
        return 0
    except Exception as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
