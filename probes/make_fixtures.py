#!/usr/bin/env python3
"""Builds the crate's test fixtures: a picture, its lossless encoding, and the
samples the decoder must produce.

A lossless JPEG 2000 codestream has exactly one correct decoding, so a picture
built here, encoded by OpenJPEG through Pillow, and written out alongside its
original samples is a known answer in the strict sense: nothing in it came from
our decoder, and no tolerance applies. Run this only to add a case; the files it
writes are committed, because a test that regenerates its own expectations is
not a test.

Usage: make_fixtures.py
"""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests/fixtures"

CASES = {
    "rgb-lossless": ((20, 14), "RGB", {"irreversible": False}),
    "grey-tiled": ((20, 14), "L", {"irreversible": False, "tile_size": (8, 8)}),
    "rgb-precincts-rpcl": (
        (20, 14),
        "RGB",
        {
            "irreversible": False,
            "progression": "RPCL",
            "precinct_size": (32, 32),
            "num_resolutions": 3,
        },
    ),
}


def picture(width: int, height: int, mode: str):
    from PIL import Image

    image = Image.new(mode, (width, height))
    pixels = image.load()
    state = 99
    for y in range(height):
        for x in range(width):
            state = (state * 1103515245 + 12345) & 0xFFFFFFFF
            noise = (state >> 21) & 0x3F
            values = (
                (x * 17 + y * 5 + noise) % 256,
                (x * x + y * 11 + noise) % 256,
                ((x ^ y) * 9 + noise) % 256,
                (200 - x * 3 + noise) % 256,
            )
            pixels[x, y] = values[0] if mode == "L" else values[: len(mode)]
    return image


def main() -> int:
    FIXTURES.mkdir(parents=True, exist_ok=True)
    written = []
    for name, (size, mode, options) in CASES.items():
        image = picture(*size, mode)
        path = FIXTURES / f"{name}.j2k"
        image.save(path, format="JPEG2000", **options)
        written.append((name, size, mode, list(image.tobytes())))
        print(f"{path.name}: {path.stat().st_size} bytes")

    lines = [
        "// The samples each fixture must decode to.",
        "//",
        "// Written by `probes/make_fixtures.py`: a picture is built in",
        "// memory, encoded losslessly by an encoder that is not ours, and both the",
        "// encoded file and the original samples are written here. A lossless",
        "// codestream has exactly one correct decoding, so these are answers, not",
        "// snapshots of our own behaviour. This file is included, not compiled on",
        "// its own, so its heading is a plain comment.",
        "",
    ]
    for name, size, mode, data in written:
        identifier = name.replace("-", "_").upper()
        lines.append(f"/// {size[0]}x{size[1]} {mode}, samples interleaved.")
        lines.append(f"pub const {identifier}: &[u8] = &{data!r};")
        lines.append("")
    (FIXTURES / "expected.rs").write_text("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
