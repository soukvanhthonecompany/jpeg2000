#!/usr/bin/env python3
"""Our JPEG 2000 decoder against an independent one, image by image.

The reference is OpenJPEG, reached through Pillow. It is not this project's
code and was not written from the same reading of the standard, so agreement
between the two is evidence about the standard rather than about either
implementation's habits.

Reversible codestreams -- the 5/3 filter with no quantisation -- must agree
exactly: that is what "reversible" means, and any difference at all is a defect.
Irreversible ones are floating point on both sides and are allowed to differ by
a small amount, which is printed so it cannot drift unnoticed.

One repair is applied to the input before the reference sees it: a `TNsot` field
that disagrees with how many tile-parts a tile actually carries. OpenJPEG
refuses those files; the encoder that wrote them is wrong about a count and
right about everything else, and our decoder forgives exactly that. Patching it
for the reference is what lets the two be compared at all, and it is done to a
copy -- the bytes our decoder is given are the original, unrepaired ones.

Usage: oracle.py [--tolerance N] FILE.jp2 [FILE.jp2 ...]
"""

from __future__ import annotations

import io
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def _decoder() -> Path:
    """Whichever build of the example exists, preferring the fast one.

    A sweep over the corpus wants the release build; the controls that run in
    the gate only need *a* build, and the gate builds the debug one.
    """
    release = ROOT / "target/release/examples/decode"
    debug = ROOT / "target/debug/examples/decode"
    return release if release.exists() else debug


DECODER = _decoder()


def unwrap(data: bytes) -> bytes:
    """The raw codestream, whether or not it arrived wearing a JP2 wrapper."""
    if data[:2] == b"\xff\x4f":
        return data
    offset = 0
    while offset + 8 <= len(data):
        (size,) = struct.unpack(">I", data[offset : offset + 4])
        kind = data[offset + 4 : offset + 8]
        payload = offset + 8
        if size == 1:
            (size,) = struct.unpack(">Q", data[offset + 8 : offset + 16])
            payload = offset + 16
        elif size == 0:
            size = len(data) - offset
        if kind == b"jp2c":
            return data[payload : offset + size]
        offset += max(size, 8)
    return data


def tile_part_offsets(stream: bytes) -> list[int]:
    """Where every `SOT` marker segment starts."""
    offsets = []
    offset = 0
    while offset + 2 <= len(stream):
        if stream[offset] != 0xFF:
            offset += 1
            continue
        code = stream[offset + 1]
        if code in (0x4F, 0xD9):
            offset += 2
            continue
        if code == 0x93:  # SOD: skip to the next tile-part or the end.
            nxt = stream.find(b"\xff\x90", offset)
            end = stream.find(b"\xff\xd9", offset)
            offset = nxt if nxt >= 0 else (end if end >= 0 else len(stream))
            continue
        if offset + 4 > len(stream):
            break
        (length,) = struct.unpack(">H", stream[offset + 2 : offset + 4])
        if code == 0x90:
            offsets.append(offset)
        offset += 2 + length
    return offsets


def repair_tile_part_counts(stream: bytes) -> tuple[bytes, bool]:
    """Sets every `TNsot` to the number of tile-parts the tile really has."""
    data = bytearray(stream)
    offsets = tile_part_offsets(stream)
    counts: dict[int, int] = {}
    for offset in offsets:
        (tile,) = struct.unpack(">H", stream[offset + 4 : offset + 6])
        counts[tile] = counts.get(tile, 0) + 1
    changed = False
    for offset in offsets:
        (tile,) = struct.unpack(">H", stream[offset + 4 : offset + 6])
        if data[offset + 11] != counts[tile]:
            data[offset + 11] = counts[tile]
            changed = True
    return bytes(data), changed


def reference(path: Path):
    """OpenJPEG's answer, as a list of per-component byte planes."""
    from PIL import Image

    raw = path.read_bytes()
    stream, patched = repair_tile_part_counts(unwrap(raw))
    image = Image.open(io.BytesIO(stream), formats=["JPEG2000"])
    image.load()
    return image, patched


def ours(path: Path, output: Path) -> tuple[bool, str]:
    result = subprocess.run(
        [str(DECODER), str(path), str(output)],
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode == 0, (result.stderr or "").strip()


def compare(path: Path, tolerance: int, scratch: Path) -> tuple[str, str]:
    """Returns a verdict and a line describing it."""
    try:
        image, patched = reference(path)
    except Exception as error:  # noqa: BLE001 -- the reference's refusal is data
        return "no-reference", f"{path.name}: reference refused: {error}"

    output = scratch / f"{path.stem}.raw"
    ok, note = ours(path, output)
    if not ok:
        return "refused", f"{path.name}: we refused: {note.splitlines()[-1] if note else ''}"

    mine = output.read_bytes()
    theirs = image.tobytes()
    if len(mine) != len(theirs):
        return (
            "shape",
            f"{path.name}: {image.size} {image.mode}: "
            f"{len(mine)} bytes against {len(theirs)}",
        )
    worst = 0
    differing = 0
    for a, b in zip(mine, theirs):
        delta = abs(a - b)
        if delta:
            differing += 1
            worst = max(worst, delta)
    share = differing / max(len(mine), 1)
    detail = (
        f"{path.name}: {image.size} {image.mode}: worst {worst}, "
        f"differing {differing} of {len(mine)} ({share:.4%})"
    )
    if patched:
        detail += " [reference needed the tile-part count repaired]"
    if worst == 0:
        return "exact", detail
    if worst <= tolerance:
        return "within-tolerance", detail
    return "differs", detail


def main(argv: list[str]) -> int:
    tolerance = 0
    if "--tolerance" in argv:
        index = argv.index("--tolerance")
        tolerance = int(argv[index + 1])
        del argv[index : index + 2]
    files = [Path(name) for name in argv[1:]]
    if not files:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    if not DECODER.exists():
        print(
            "oracle: build the decoder first: "
            "cargo build --release --example decode",
            file=sys.stderr,
        )
        return 2

    # Decoded samples are large and are never an answer on their own; they go
    # somewhere temporary rather than beside the probe.
    scratch = Path(tempfile.mkdtemp(prefix="jpeg2000-oracle-"))
    tally: dict[str, int] = {}
    for path in files:
        verdict, line = compare(path, tolerance, scratch)
        tally[verdict] = tally.get(verdict, 0) + 1
        marker = {"exact": "  ", "within-tolerance": "~ "}.get(verdict, "! ")
        print(f"{marker}{verdict}: {line}")
    print("\n== " + ", ".join(f"{k} {v}" for k, v in sorted(tally.items())))
    return 0 if set(tally) <= {"exact", "within-tolerance", "no-reference"} else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
