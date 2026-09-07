#!/usr/bin/env python3
"""
Package the Windows Runtime Bundle for Hands.

Stages a self-contained runtime bundle beside the Hands executable (or in a specified
staging directory) with pinned rg.exe, hands.exe, tunnel-client.exe, a cryptographic
manifest (manifest.json), SHA256SUMS.txt, and provenance evidence.

Contract from Issue #62 and ADR-0002:
- The Runtime Bundle lives under `runtime/<version>/` (or staging target).
- Complete runtime bundle composition: hands.exe + tunnel-client.exe + pinned rg.exe.
- Pinned `rg.exe` MUST sit beside `hands.exe` and `tunnel-client.exe` in the bundle.
- Packaging fails closed if rg.exe does not match the pinned version and SHA-256.
- `--verify-only` enforces complete composition, manifest evidence, and SHA256 verification.
- Verified without relying on %LOCALAPPDATA% or PATH.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import struct
import sys
from pathlib import Path

# Pinned ripgrep metadata for Windows x86_64
PINNED_RG = {
    "version": "15.1.0",
    "target": "x86_64-pc-windows-msvc",
    "filename": "rg.exe",
    "sha256": "decdd4992f3f1b9a5ef9898f1b40ab16886d579d6516b4efd3d5eaa19364e408",
    "license": "MIT OR Unlicense",
    "upstream": "https://github.com/BurntSushi/ripgrep",
}

REQUIRED_BUNDLE_FILES = {"rg.exe", "hands.exe", "tunnel-client.exe"}

# The Windows Portable Runtime Bundle intentionally remains a three-binary
# artifact. A default MSVC Rust build may import VCRUNTIME/MSVCP DLLs that are
# present on a developer workstation but absent in a clean Windows Sandbox.
# Fail closed rather than producing a checksum-valid bundle that cannot start.
DYNAMIC_MSVC_CRT_RE = re.compile(r"^(?:vcruntime|msvcp)\d+(?:_\d+)?\.dll$", re.IGNORECASE)


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(65536):
            h.update(chunk)
    return h.hexdigest()


def parse_pe_imports_and_arch(path: Path) -> tuple[int, list[str]]:
    """Parse PE headers, validate x86_64 architecture, and extract imported DLL names."""
    data = path.read_bytes()
    if len(data) < 64 or data[:2] != b"MZ":
        raise RuntimeError(f"{path.name} is not a valid PE executable (missing MZ header)")
    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if pe_offset + 24 > len(data) or data[pe_offset:pe_offset+4] != b"PE\0\0":
        raise RuntimeError(f"{path.name} is not a valid PE executable (missing PE signature)")
    machine, num_sections = struct.unpack_from("<HH", data, pe_offset + 4)
    if machine != 0x8664:
        raise RuntimeError(
            f"{path.name} has wrong machine architecture: expected x86_64 (0x8664), got 0x{machine:04x}"
        )
    size_of_opt_hdr = struct.unpack_from("<H", data, pe_offset + 20)[0]
    opt_offset = pe_offset + 24
    if size_of_opt_hdr < 112:
        raise RuntimeError(
            f"{path.name} has invalid PE32+ optional header size: expected at least 112, got {size_of_opt_hdr}"
        )
    if opt_offset + size_of_opt_hdr > len(data):
        raise RuntimeError(f"{path.name} is truncated (optional header extends past EOF)")
    magic = struct.unpack_from("<H", data, opt_offset)[0]
    if magic != 0x020B:
        raise RuntimeError(f"{path.name} is not a PE32+ (64-bit) executable: magic 0x{magic:04x}")
    num_rva_and_sizes = struct.unpack_from("<I", data, opt_offset + 108)[0]
    if size_of_opt_hdr < 112 + num_rva_and_sizes * 8:
        raise RuntimeError(
            f"{path.name} has malformed optional header: size {size_of_opt_hdr} cannot fit {num_rva_and_sizes} data directories"
        )
    import_rva, import_size = 0, 0
    if num_rva_and_sizes >= 2:
        import_rva, import_size = struct.unpack_from("<II", data, opt_offset + 120)

    sections_offset = opt_offset + size_of_opt_hdr
    if sections_offset + num_sections * 40 > len(data):
        raise RuntimeError(f"{path.name} is truncated (section table extends past EOF)")

    sections = []
    for i in range(num_sections):
        s_off = sections_offset + i * 40
        v_size, v_addr, raw_size, raw_ptr = struct.unpack_from("<IIII", data, s_off + 8)
        if raw_size > 0 and raw_ptr + raw_size > len(data):
            raise RuntimeError(f"{path.name} is truncated (section raw data extends past EOF)")
        sections.append((v_addr, max(v_size, raw_size), raw_ptr))

    def rva_to_offset(rva: int) -> int | None:
        for v_addr, size, raw_ptr in sections:
            if v_addr <= rva < v_addr + size:
                return raw_ptr + (rva - v_addr)
        return None

    imported_dlls = []
    if import_rva and import_size:
        imp_off = rva_to_offset(import_rva)
        if imp_off is None:
            raise RuntimeError(
                f"{path.name} has malformed import directory: RVA 0x{import_rva:08x} not in any section"
            )
        found_null_descriptor = False
        while imp_off + 20 <= len(data):
            desc = struct.unpack_from("<IIIII", data, imp_off)
            if desc == (0, 0, 0, 0, 0):
                found_null_descriptor = True
                break
            name_rva = desc[3]
            if not name_rva:
                raise RuntimeError(
                    f"{path.name} has malformed import directory descriptor: missing name RVA"
                )
            name_off = rva_to_offset(name_rva)
            if name_off is None:
                raise RuntimeError(
                    f"{path.name} has malformed import directory: DLL name RVA 0x{name_rva:08x} not in any section"
                )
            if name_off >= len(data):
                raise RuntimeError(
                    f"{path.name} is truncated (import DLL name offset extends past EOF)"
                )
            end = data.find(b"\0", name_off)
            if end == -1:
                raise RuntimeError(
                    f"{path.name} has malformed import directory: unterminated DLL name string"
                )
            dll_name = data[name_off:end].decode("ascii", "replace")
            imported_dlls.append(dll_name)
            imp_off += 20
        if not found_null_descriptor:
            raise RuntimeError(
                f"{path.name} has truncated import directory table (missing null terminator descriptor)"
            )
    return machine, imported_dlls


def verify_pe_x86_64(path: Path, binary_name: str) -> None:
    """Verify that a binary has valid PE headers and targets x86_64."""
    parse_pe_imports_and_arch(path)


def verify_hands_static_crt(path: Path) -> None:
    """Reject a Hands PE that still imports an app-external MSVC runtime DLL."""
    _, imported_dlls = parse_pe_imports_and_arch(path)
    bad_imports = sorted({dll for dll in imported_dlls if DYNAMIC_MSVC_CRT_RE.match(dll)})
    if bad_imports:
        joined = ", ".join(bad_imports)
        raise RuntimeError(
            "hands.exe imports dynamic MSVC CRT DLL(s): "
            f"{joined}. The three-file Windows Runtime Bundle must start on a clean machine. "
            "Build Hands with static CRT linkage (for example, "
            "RUSTFLAGS='-C target-feature=+crt-static') before packaging."
        )


def make_dummy_pe(dlls: list[str] = ()) -> bytes:
    """Construct a minimal valid x86_64 PE binary for testing."""
    dos = bytearray(64)
    dos[:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, 64)
    pe_sig = b"PE\0\0"
    file_hdr = struct.pack("<HHIIIHH", 0x8664, 1, 0, 0, 0, 240, 0x22)
    opt_hdr = bytearray(240)
    struct.pack_into("<H", opt_hdr, 0, 0x020B)
    struct.pack_into("<II", opt_hdr, 32, 0x1000, 0x200)
    struct.pack_into("<I", opt_hdr, 108, 16)
    sec_data = bytearray()
    if dlls:
        desc_size = (len(dlls) + 1) * 20
        name_offset_in_sec = desc_size
        names_data = bytearray()
        desc_data = bytearray()
        for d in dlls:
            d_bytes = d.encode("ascii") + b"\0"
            name_rva = 0x1000 + name_offset_in_sec + len(names_data)
            names_data.extend(d_bytes)
            desc_data.extend(struct.pack("<IIIII", 0, 0, 0, name_rva, 0))
        desc_data.extend(b"\0" * 20)
        sec_data = desc_data + names_data
        struct.pack_into("<II", opt_hdr, 120, 0x1000, len(sec_data))
    raw_size = ((len(sec_data) + 511) // 512) * 512
    if raw_size == 0:
        raw_size = 512
    sec_data.extend(b"\0" * (raw_size - len(sec_data)))
    sec_hdr = bytearray(40)
    sec_hdr[:5] = b".text"
    struct.pack_into("<IIII", sec_hdr, 8, max(len(sec_data), 0x1000), 0x1000, raw_size, 512)
    headers = dos + pe_sig + file_hdr + opt_hdr + sec_hdr
    headers.extend(b"\0" * (512 - len(headers)))
    return bytes(headers + sec_data)


def _verify_bundle_core(out_dir: Path, expected_rg_hash: str) -> dict:
    manifest_path = out_dir / "manifest.json"
    checksums_path = out_dir / "SHA256SUMS.txt"

    if not manifest_path.is_file():
        raise RuntimeError(f"Manifest missing at {manifest_path}")
    if not checksums_path.is_file():
        raise RuntimeError(f"Checksums file missing at {checksums_path}")

    with open(manifest_path, "r", encoding="utf-8") as f:
        manifest = json.load(f)

    manifest_file_names = {item["name"] for item in manifest.get("files", [])}
    missing_required = REQUIRED_BUNDLE_FILES - manifest_file_names
    if missing_required:
        raise RuntimeError(
            f"Bundle manifest missing required files: {sorted(missing_required)}. "
            f"Expected complete composition: {sorted(REQUIRED_BUNDLE_FILES)}"
        )

    # Parse SHA256SUMS.txt entries
    checksums = {}
    with open(checksums_path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.split(maxsplit=1)
            if len(parts) == 2:
                name = parts[1].lstrip("*")
                checksums[name] = parts[0]

    for req in REQUIRED_BUNDLE_FILES:
        if req not in checksums:
            raise RuntimeError(f"Required file {req} not found in SHA256SUMS.txt")

    for item in manifest.get("files", []):
        file_path = out_dir / item["name"]
        if not file_path.is_file():
            raise RuntimeError(f"Required bundle file missing on disk: {file_path}")

        actual_hash = sha256_file(file_path)
        if actual_hash != item["sha256"]:
            raise RuntimeError(
                f"Checksum mismatch for {item['name']} in manifest: "
                f"expected {item['sha256']}, got {actual_hash}"
            )

        if checksums.get(item["name"]) != actual_hash:
            raise RuntimeError(
                f"Checksum mismatch for {item['name']} in SHA256SUMS.txt: "
                f"expected {actual_hash}, got {checksums.get(item['name'])}"
            )

        if item["name"] == "rg.exe":
            if actual_hash != expected_rg_hash:
                raise RuntimeError(
                    f"Pinned rg.exe hash mismatch: expected {expected_rg_hash}, got {actual_hash}"
                )
        elif item["name"] == "hands.exe":
            verify_hands_static_crt(file_path)
            if item.get("crt_linkage") != "static":
                raise RuntimeError(
                    "hands.exe manifest must declare crt_linkage=static for the portable Windows bundle"
                )

    return manifest


def verify_bundle(out_dir: Path) -> dict:
    return _verify_bundle_core(out_dir, expected_rg_hash=PINNED_RG["sha256"])


def verify_bundle_for_testing(out_dir: Path, expected_rg_hash: str = PINNED_RG["sha256"]) -> dict:
    return _verify_bundle_core(out_dir, expected_rg_hash=expected_rg_hash)


def _stage_bundle_core(
    out_dir: Path,
    hands_bin: Path | None = None,
    rg_bin: Path | None = None,
    tunnel_client_bin: Path | None = None,
    version: str = "0.1.0",
    expected_rg_hash: str = PINNED_RG["sha256"],
) -> dict:
    out_dir = out_dir.resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    manifest_path = out_dir / "manifest.json"
    checksums_path = out_dir / "SHA256SUMS.txt"
    manifest_files = []

    # 1. Stage rg.exe (fail closed if hash does not match pin)
    dest_rg = out_dir / "rg.exe"
    if rg_bin is not None:
        if not rg_bin.is_file():
            raise RuntimeError(f"Explicit rg-bin path does not exist or is not a file: {rg_bin}")
        shutil.copy2(rg_bin, dest_rg)
    elif not dest_rg.is_file():
        raise RuntimeError(
            f"rg.exe not provided and not present at {dest_rg}. "
            "A pinned rg.exe is mandatory for the Windows Runtime Bundle."
        )

    actual_rg_hash = sha256_file(dest_rg)
    if actual_rg_hash != expected_rg_hash:
        raise RuntimeError(
            f"rg.exe failed pinning validation: expected SHA256 {expected_rg_hash}, "
            f"got {actual_rg_hash}. Packaging rejected."
        )

    manifest_files.append({
        "name": "rg.exe",
        "size": dest_rg.stat().st_size,
        "sha256": actual_rg_hash,
        "version": PINNED_RG["version"],
        "pinned_sha256": expected_rg_hash,
        "license": PINNED_RG["license"],
        "source": PINNED_RG["upstream"],
    })

    # 2. Stage hands.exe
    dest_hands = out_dir / "hands.exe"
    if hands_bin is not None:
        if not hands_bin.is_file():
            raise RuntimeError(f"Explicit hands-bin path does not exist or is not a file: {hands_bin}")
        shutil.copy2(hands_bin, dest_hands)
    elif not dest_hands.is_file():
        raise RuntimeError(
            f"hands.exe not provided and not present at {dest_hands}. "
            "hands.exe is mandatory for the Windows Runtime Bundle."
        )

    verify_hands_static_crt(dest_hands)

    manifest_files.append({
        "name": "hands.exe",
        "size": dest_hands.stat().st_size,
        "sha256": sha256_file(dest_hands),
        "version": version,
        "license": "Apache-2.0",
        "crt_linkage": "static",
    })

    # 3. Stage tunnel-client.exe
    dest_tc = out_dir / "tunnel-client.exe"
    if tunnel_client_bin is not None:
        if not tunnel_client_bin.is_file():
            raise RuntimeError(f"Explicit tunnel-client-bin path does not exist or is not a file: {tunnel_client_bin}")
        shutil.copy2(tunnel_client_bin, dest_tc)
    elif not dest_tc.is_file():
        raise RuntimeError(
            f"tunnel-client.exe not provided and not present at {dest_tc}. "
            "tunnel-client.exe is mandatory for the complete Windows Runtime Bundle composition."
        )

    verify_pe_x86_64(dest_tc, "tunnel-client.exe")

    manifest_files.append({
        "name": "tunnel-client.exe",
        "size": dest_tc.stat().st_size,
        "sha256": sha256_file(dest_tc),
        "license": "Proprietary",
    })

    # 4. Generate manifest.json
    manifest = {
        "schema_version": "1.0.0",
        "bundle_version": version,
        "target_os": "windows",
        "target_arch": "x86_64",
        "files": manifest_files,
    }
    with open(manifest_path, "w", encoding="utf-8") as f:
        json.dump(manifest, f, indent=2)

    # 5. Generate SHA256SUMS.txt
    with open(checksums_path, "w", encoding="utf-8") as f:
        for item in manifest_files:
            f.write(f"{item['sha256']} *{item['name']}\n")

    return manifest


def stage_bundle(
    out_dir: Path,
    hands_bin: Path | None = None,
    rg_bin: Path | None = None,
    tunnel_client_bin: Path | None = None,
    version: str = "0.1.0",
) -> dict:
    return _stage_bundle_core(
        out_dir=out_dir,
        hands_bin=hands_bin,
        rg_bin=rg_bin,
        tunnel_client_bin=tunnel_client_bin,
        version=version,
        expected_rg_hash=PINNED_RG["sha256"],
    )


def stage_bundle_for_testing(
    out_dir: Path,
    hands_bin: Path | None = None,
    rg_bin: Path | None = None,
    tunnel_client_bin: Path | None = None,
    version: str = "0.1.0",
    expected_rg_hash: str = PINNED_RG["sha256"],
) -> dict:
    return _stage_bundle_core(
        out_dir=out_dir,
        hands_bin=hands_bin,
        rg_bin=rg_bin,
        tunnel_client_bin=tunnel_client_bin,
        version=version,
        expected_rg_hash=expected_rg_hash,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description="Package Windows Runtime Bundle for Hands")
    parser.add_argument("--out-dir", type=Path, required=True, help="Destination directory for bundle")
    parser.add_argument("--hands-bin", type=Path, default=None, help="Path to hands.exe binary")
    parser.add_argument("--rg-bin", type=Path, default=None, help="Path to pinned rg.exe binary")
    parser.add_argument("--tunnel-client-bin", type=Path, default=None, help="Path to tunnel-client.exe")
    parser.add_argument("--version", type=str, default="0.1.0", help="Runtime version string")
    parser.add_argument("--verify-only", action="store_true", help="Verify existing bundle manifest and checksums")

    args = parser.parse_args()
    try:
        if args.verify_only:
            manifest = verify_bundle(args.out_dir)
            print(f"Bundle successfully verified at {args.out_dir}")
        else:
            manifest = stage_bundle(
                out_dir=args.out_dir,
                hands_bin=args.hands_bin,
                rg_bin=args.rg_bin,
                tunnel_client_bin=args.tunnel_client_bin,
                version=args.version,
            )
            print(f"Bundle successfully staged at {args.out_dir}")

        print(json.dumps(manifest, indent=2))
        return 0
    except Exception as e:
        print(f"Packaging error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
