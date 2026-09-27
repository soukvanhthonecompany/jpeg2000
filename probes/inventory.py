#!/usr/bin/env python3
"""What the corpus's JPEG 2000 images actually use.

A decoder written from the standard alone decodes every legal file and takes
forever to write. A decoder written from a corpus decodes the corpus and lies
about the rest. This prints the difference: for every `/JPXDecode` stream in
the files given, the markers that are present and the options each one selects.

It reads the stream bytes without decoding them, so it says nothing about
whether an image is correct -- only about which parts of Part 1 a decoder has
to implement before any of these can be drawn at all.

Usage: inventory.py FILE.pdf [FILE.pdf ...]
"""

from __future__ import annotations

import re
import struct
import sys
from pathlib import Path

PROGRESSION = {0: "LRCP", 1: "RLCP", 2: "RPCL", 3: "PCRL", 4: "CPRL"}
TRANSFORM = {0: "9/7 irreversible", 1: "5/3 reversible"}
QUANTISATION = {0: "none (reversible)", 1: "scalar derived", 2: "scalar expounded"}

# Part 1 marker segments, by their second byte. Anything absent from this table
# is reported by its hex code rather than silently skipped.
MARKERS = {
    0x4F: "SOC", 0x51: "SIZ", 0x52: "COD", 0x53: "COC", 0x5C: "QCD",
    0x5D: "QCC", 0x5E: "RGN", 0x5F: "POC", 0x55: "TLM", 0x57: "PLM",
    0x58: "PLT", 0x60: "PPM", 0x61: "PPT", 0x63: "CRG", 0x64: "COM",
    0x90: "SOT", 0x91: "SOP", 0x92: "EPH", 0x93: "SOD", 0xD9: "EOC",
}
# Markers with no length field.
BARE = {0x4F, 0x93, 0xD9, 0x92}


def streams(pdf: bytes):
    """Every `/JPXDecode` stream body in a file, found without a PDF parser.

    The filter is the whole pipeline for these images -- a `/JPXDecode` stream
    carries the codestream verbatim -- so the bytes between `stream` and the
    declared `/Length` are the thing itself.
    """
    for match in re.finditer(rb"/JPXDecode", pdf):
        start = pdf.rfind(b"obj", 0, match.start())
        head = pdf.find(b"stream", match.start())
        if start < 0 or head < 0:
            continue
        length = re.search(rb"/Length\s+(\d+)", pdf[start:head])
        body = head + len(b"stream")
        if pdf[body : body + 2] == b"\r\n":
            body += 2
        elif pdf[body : body + 1] in (b"\n", b"\r"):
            body += 1
        if length is None:
            end = pdf.find(b"endstream", body)
            yield pdf[body:end]
        else:
            yield pdf[body : body + int(length.group(1))]


def codestream(data: bytes) -> bytes:
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


def read(stream: bytes) -> dict:
    """Every marker segment before the first tile-part's data, plus counts."""
    report = {"markers": {}, "tiles": 0, "unknown": []}
    offset = 0
    while offset + 2 <= len(stream):
        if stream[offset] != 0xFF:
            offset += 1
            continue
        code = stream[offset + 1]
        name = MARKERS.get(code)
        if name is None:
            report["unknown"].append(f"ff{code:02x}")
            offset += 2
            continue
        report["markers"][name] = report["markers"].get(name, 0) + 1
        if code in BARE:
            offset += 2
            if code == 0x93:  # SOD: tile-part data runs to the next SOT or EOC.
                nxt = stream.find(b"\xff\x90", offset)
                end = stream.find(b"\xff\xd9", offset)
                offset = nxt if nxt >= 0 else (end if end >= 0 else len(stream))
            continue
        if code == 0x91:  # SOP is bare in the sense that it never precedes data.
            offset += 2
            continue
        (length,) = struct.unpack(">H", stream[offset + 2 : offset + 4])
        segment = stream[offset + 4 : offset + 2 + length]
        if name == "SIZ":
            report["siz"] = siz(segment)
        elif name == "COD":
            report["cod"] = cod(segment)
        elif name == "QCD":
            report["qcd"] = qcd(segment)
        elif name == "SOT":
            report["tiles"] += 1
        offset += 2 + length
    return report


def siz(segment: bytes) -> dict:
    (_, xsiz, ysiz, xo, yo, xt, yt, xto, yto, csiz) = struct.unpack(
        ">HIIIIIIIIH", segment[:36]
    )
    components = []
    for index in range(csiz):
        ssiz, xr, yr = segment[36 + index * 3 : 39 + index * 3]
        components.append(
            {"depth": (ssiz & 0x7F) + 1, "signed": bool(ssiz & 0x80), "dx": xr, "dy": yr}
        )
    return {
        "size": (xsiz - xo, ysiz - yo),
        "origin": (xo, yo),
        "tile": (xt, yt),
        "tile_origin": (xto, yto),
        "components": components,
    }


def cod(segment: bytes) -> dict:
    scod = segment[0]
    progression, layers, mct = struct.unpack(">BHB", segment[1:5])
    levels, cbw, cbh, style, transform = segment[5:10]
    precincts = list(segment[10:]) if scod & 1 else []
    return {
        "progression": PROGRESSION.get(progression, progression),
        "layers": layers,
        "mct": bool(mct),
        "levels": levels,
        "code_block": (1 << ((cbw & 0xF) + 2), 1 << ((cbh & 0xF) + 2)),
        "block_style": style,
        "transform": TRANSFORM.get(transform, transform),
        "precincts": [(1 << (p & 0xF), 1 << (p >> 4)) for p in precincts],
        "sop": bool(scod & 2),
        "eph": bool(scod & 4),
    }


def qcd(segment: bytes) -> dict:
    sqcd = segment[0]
    return {"style": QUANTISATION.get(sqcd & 0x1F, sqcd & 0x1F), "guard_bits": sqcd >> 5}


def tally(reports: list[dict]) -> None:
    """One line per option, with how many of the codestreams choose each value.

    This is the list a decoder is written against. An option with one value
    across the whole corpus still has to be *read*, because the next file may
    choose the other one -- but it says which paths can be written last.
    """
    counts: dict[str, dict[str, int]] = {}

    def note(option: str, value: object) -> None:
        counts.setdefault(option, {})
        key = str(value)
        counts[option][key] = counts[option].get(key, 0) + 1

    for report in reports:
        s, c, q = report.get("siz"), report.get("cod"), report.get("qcd")
        if s:
            note("components", len(s["components"]))
            note("depth", sorted({(x["depth"], x["signed"]) for x in s["components"]}))
            note("subsampling", sorted({(x["dx"], x["dy"]) for x in s["components"]}))
            note("image origin", s["origin"])
            note(
                "tiled",
                "one tile" if s["tile"] >= s["size"] else f"tiles of {s['tile']}",
            )
            note("tile origin", s["tile_origin"])
        if c:
            for key in ("progression", "layers", "mct", "levels", "code_block",
                        "transform", "sop", "eph"):
                note(key, c[key])
            note("block style", f"0x{c['block_style']:02x}")
            note("precincts", "default" if not c["precincts"] else "declared")
        if q:
            note("quantisation", q["style"])
            note("guard bits", q["guard_bits"])
        note("tile-parts per image", report["tiles"])
        for marker in ("POC", "PPM", "PPT", "RGN", "COC", "QCC", "TLM", "PLT", "CRG"):
            note(f"uses {marker}", marker in report["markers"])

    print(f"\n== {len(reports)} codestreams")
    for option, values in counts.items():
        shown = ", ".join(
            f"{value} x{count}"
            for value, count in sorted(values.items(), key=lambda kv: -kv[1])
        )
        print(f"  {option}: {shown}")


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    summary = "--summary" in argv
    argv = [a for a in argv if a != "--summary"]
    collected: list[dict] = []
    for name in argv[1:]:
        pdf = Path(name).read_bytes()
        # A file that is not a PDF is taken to be one codestream, so the same
        # reader serves a document and a stream already lifted out of one.
        found = [pdf] if pdf[:5] != b"%PDF-" else list(streams(pdf))
        if not summary:
            print(f"\n== {Path(name).name}: {len(found)} /JPXDecode streams")
        for index, body in enumerate(found):
            report = read(codestream(body))
            collected.append(report)
            if summary:
                continue
            s, c, q = report.get("siz"), report.get("cod"), report.get("qcd")
            print(f"  [{index}] {len(body)} bytes, tile-parts {report['tiles']}")
            if s:
                depths = {(x["depth"], x["signed"]) for x in s["components"]}
                sub = {(x["dx"], x["dy"]) for x in s["components"]}
                print(
                    f"      {s['size'][0]}x{s['size'][1]} "
                    f"{len(s['components'])} components, depth {sorted(depths)}, "
                    f"subsampling {sorted(sub)}, tile {s['tile']}"
                )
            if c:
                print(
                    f"      {c['progression']}, {c['layers']} layers, MCT {c['mct']}, "
                    f"{c['levels']} levels, blocks {c['code_block']}, "
                    f"style 0x{c['block_style']:02x}, {c['transform']}"
                )
                print(
                    f"      precincts {c['precincts'] or 'default'}, "
                    f"SOP {c['sop']}, EPH {c['eph']}"
                )
            if q:
                print(f"      quantisation {q['style']}, {q['guard_bits']} guard bits")
            present = ", ".join(f"{k}x{v}" for k, v in sorted(report["markers"].items()))
            print(f"      markers: {present}")
            if report["unknown"]:
                print(f"      unknown: {sorted(set(report['unknown']))}")
    if summary:
        tally(collected)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
