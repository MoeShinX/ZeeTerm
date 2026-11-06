#!/usr/bin/env python3
"""Verify the pinned upstream archive and the narrowly scoped local patch."""
import hashlib
from pathlib import Path
import sys
import tarfile

EXPECTED = "0d8c1fef690941d3e7788d328517591fecc684c084084702d6ff1641e993699a"
ROOT = Path(__file__).resolve().parent


def main():
    if len(sys.argv) != 2:
        raise ValueError("usage: verify-source.py /path/to/block-0.1.6.crate")
    archive = Path(sys.argv[1])
    if hashlib.sha256(archive.read_bytes()).hexdigest() != EXPECTED:
        raise ValueError("upstream archive SHA-256 mismatch")
    names = ["Cargo.toml", "README.md", "src/lib.rs", "src/test_utils.rs"]
    with tarfile.open(archive, "r:gz") as source:
        if sorted(source.getnames()) != sorted("block-0.1.6/" + n for n in names):
            raise ValueError("unexpected archive members")
        for name in names:
            member = source.getmember("block-0.1.6/" + name)
            if not member.isfile():
                raise ValueError("expected regular archive member")
            stream = source.extractfile(member)
            if stream is None:
                raise ValueError("archive member is unreadable")
            original = stream.read()
            if name == "src/lib.rs":
                original = original.replace(
                    b"enum Class { }\n",
                    b"// An opaque FFI marker must be inhabited so newer Rust versions can accept\n"
                    b"// references to foreign statics without treating them as impossible values.\n"
                    b"#[repr(C)]\nstruct Class {\n    _private: [u8; 0],\n}\n",
                ).replace(b"extern ", b'extern "C" ')
                original = (
                    b"// Adapted from SSheldon's rust-block, crates.io block 0.1.6 (MIT); see PROVENANCE.\n"
                    + original
                )
            if (ROOT / name).read_bytes() != original:
                raise ValueError("unexpected local changes: " + name)
    print("PASS: pinned archive and all four source files match the declared patch")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, tarfile.TarError) as error:
        sys.exit(str(error))
