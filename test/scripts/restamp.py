#!/usr/bin/env python3
"""restamp.py — retarget a built extension at a different DuckDB version.

    test/scripts/restamp.py in.duckdb_extension v2.0.0-alpha37626 out.duckdb_extension
    test/scripts/restamp.py --read some.duckdb_extension

Harbor is Rust over DuckDB's C API and never links DuckDB, so the compiled
payload does not depend on which DuckDB it will be loaded into. The only thing
that does is the version DuckDB checks at load time, which lives in a 32-byte
field in the metadata trailer. Rewriting that field produces, byte for byte,
what building with `make release TARGET_DUCKDB_VERSION=...` produces — verified
by `--verify-against`, and worth re-verifying whenever the toolchain moves.

This exists so a release does not have to build the same code twice per
platform. CI builds the stable set; this makes the alpha set from it.

The trailer is eight 32-byte fields at the end of the file, most significant
last, so FIELD3 (duckdb_version) sits 352 bytes from EOF. It is followed by a
256-byte signature area that is zero for an unsigned extension, which is what
every harbor build is.
"""

import argparse
import sys

FIELD_SIZE = 32
# FIELD8..FIELD1 then the 256-byte signature: FIELD3 starts 11 fields from the
# end (8 fields + 256 bytes of signature = 512, and FIELD3 is the third from
# the bottom of the eight).
DUCKDB_VERSION_OFFSET = -352


def read_version(blob: bytes) -> str:
    field = blob[DUCKDB_VERSION_OFFSET:DUCKDB_VERSION_OFFSET + FIELD_SIZE]
    return field.rstrip(b"\x00").decode("utf-8", "replace")


def restamp(blob: bytes, version: str) -> bytes:
    encoded = version.encode()
    if len(encoded) > FIELD_SIZE:
        raise SystemExit(
            f"restamp: {version!r} is {len(encoded)} bytes; the field holds {FIELD_SIZE}"
        )
    field = encoded.ljust(FIELD_SIZE, b"\x00")
    start = len(blob) + DUCKDB_VERSION_OFFSET
    return blob[:start] + field + blob[start + FIELD_SIZE:]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("input")
    ap.add_argument("version", nargs="?")
    ap.add_argument("output", nargs="?")
    ap.add_argument("--read", action="store_true", help="print the stamped version and exit")
    ap.add_argument(
        "--verify-against",
        metavar="FILE",
        help="a natively built extension for the same version; the result must match it exactly",
    )
    args = ap.parse_args()

    blob = open(args.input, "rb").read()
    if args.read:
        print(read_version(blob))
        return 0
    if not args.version or not args.output:
        ap.error("version and output are required unless --read")

    stamped = restamp(blob, args.version)

    if args.verify_against:
        native = open(args.verify_against, "rb").read()
        if stamped != native:
            differ = sum(1 for a, b in zip(stamped, native) if a != b)
            print(
                f"restamp: the restamped file is NOT identical to {args.verify_against} "
                f"({differ} bytes differ, lengths {len(stamped)} and {len(native)}). "
                "The payload now depends on the target version, so restamping is no "
                "longer sound — build each version natively instead.",
                file=sys.stderr,
            )
            return 1
        print(f"restamp: byte-for-byte identical to {args.verify_against}")

    open(args.output, "wb").write(stamped)
    print(f"restamp: {args.input} [{read_version(blob)}] -> {args.output} [{args.version}]")
    return 0


if __name__ == "__main__":
    sys.exit(main())
