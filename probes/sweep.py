#!/usr/bin/env python3
"""Builds codestreams across the option space and checks every one of them.

The corpus exercises a narrow slice of Part 1: one tiling style, two
progressions, two filters, default precincts, no subsampling, eight bits. A
decoder measured only against that slice is a decoder that works on six files.
So this encodes the same picture again and again with the options changed one
at a time, and hands each result to `oracle.py`.

What it cannot reach it says so about: Pillow drives OpenJPEG's encoder, which
will not emit the selective-bypass or terminate-all code-block styles, region
of interest, or component subsampling. Those paths are written from the
standard and are covered by unit tests inside the crate instead -- this file
does not pretend otherwise.

Usage: sweep.py [--keep DIR]
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ORACLE = Path(__file__).resolve().parent / "oracle.py"


def picture(width: int, height: int, mode: str):
    """A picture with edges, gradients and noise, so no subband is empty."""
    from PIL import Image

    image = Image.new(mode, (width, height))
    pixels = image.load()
    state = 1234567
    for y in range(height):
        for x in range(width):
            state = (state * 1103515245 + 12345) & 0xFFFFFFFF
            noise = (state >> 20) & 0x1F
            ramp = (x * 255) // max(width - 1, 1)
            edge = 255 if (x // 13 + y // 11) % 2 else 0
            values = (
                (ramp + noise) % 256,
                (edge + noise) % 256,
                ((x * y) // 7 + noise) % 256,
                (255 - ramp + noise) % 256,
            )
            pixels[x, y] = values[0] if mode == "L" else values[: len(mode)]
    return image


def cases() -> list[tuple[str, str, dict]]:
    """Every option this encoder can be made to vary, one at a time."""
    lossless = {"irreversible": False}
    lossy = {"irreversible": True, "quality_mode": "rates", "quality_layers": [1]}
    listed = [
        ("baseline-lossless", "RGB", lossless),
        ("baseline-lossy", "RGB", lossy),
        ("greyscale", "L", lossless),
        ("alpha", "RGBA", lossless),
        ("no-colour-transform", "RGB", {**lossless, "mct": 0}),
        ("one-resolution", "RGB", {**lossless, "num_resolutions": 1}),
        ("two-resolutions", "RGB", {**lossless, "num_resolutions": 2}),
        ("seven-resolutions", "RGB", {**lossless, "num_resolutions": 7}),
        ("small-code-blocks", "RGB", {**lossless, "codeblock_size": (16, 16)}),
        ("wide-code-blocks", "RGB", {**lossless, "codeblock_size": (64, 16)}),
        ("large-code-blocks", "RGB", {**lossless, "codeblock_size": (64, 64)}),
        ("precincts", "RGB", {**lossless, "precinct_size": (128, 128)}),
        ("small-precincts", "RGB", {**lossless, "precinct_size": (64, 64)}),
        ("tiles", "RGB", {**lossless, "tile_size": (64, 64)}),
        ("tiles-offset", "RGB", {**lossless, "tile_size": (64, 64), "tile_offset": (0, 0)}),
        ("tiles-and-precincts", "RGB", {**lossless, "tile_size": (64, 64), "precinct_size": (32, 32)}),
        ("many-layers", "RGB", {**lossless, "quality_layers": [0, 0, 0, 0, 0, 0]}),
        ("lossy-layers", "RGB", {**lossy, "quality_layers": [40, 20, 10, 5, 2, 1]}),
        ("lossy-tiles", "RGB", {**lossy, "tile_size": (64, 64)}),
        ("lossy-precincts", "RGB", {**lossy, "precinct_size": (64, 64)}),
        ("lossy-greyscale", "L", lossy),
        ("jp2-wrapper", "RGB", {**lossless, "no_jp2": False}),
    ]
    for name in ("LRCP", "RLCP", "RPCL", "PCRL", "CPRL"):
        listed.append((f"progression-{name.lower()}", "RGB", {**lossless, "progression": name}))
        listed.append(
            (
                f"progression-{name.lower()}-tiled",
                "RGB",
                {**lossless, "progression": name, "tile_size": (64, 64), "precinct_size": (32, 32)},
            )
        )
    return listed


def main(argv: list[str]) -> int:
    keep = None
    if "--keep" in argv:
        keep = Path(argv[argv.index("--keep") + 1])
        keep.mkdir(parents=True, exist_ok=True)
    directory = keep or Path(tempfile.mkdtemp(prefix="jpeg2000-sweep-"))

    written = []
    for name, mode, options in cases():
        image = picture(157, 113, mode)
        suffix = "jp2" if options.get("no_jp2") is False else "j2k"
        path = directory / f"{name}.{suffix}"
        try:
            image.save(path, format="JPEG2000", **options)
        except Exception as error:  # noqa: BLE001 -- an encoder's refusal is data
            print(f"! could not encode {name}: {error}")
            continue
        written.append(path)
    print(f"encoded {len(written)} of {len(cases())} cases into {directory}\n")

    result = subprocess.run(
        [sys.executable, str(ORACLE), "--tolerance", "3", *[str(p) for p in written]],
        cwd=ROOT,
        check=False,
    )
    return result.returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv))
