#!/usr/bin/env python3
"""Controls for the JPEG 2000 probes.

G11: an instrument reports nothing until it is calibrated. These check the two
files that measure our decoder -- the comparison against OpenJPEG, and the
inventory that says what the corpus uses -- against answers known without
either of them.

The comparison's most important property is that it can fail. A checker that
says "exact" about everything would have reported a passing sweep all along,
so the negative controls here are the point: a decoder whose output is one byte
wrong, one byte short, or absent must each come back as something other than
agreement.
"""

from __future__ import annotations

import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import inventory  # noqa: E402
import oracle  # noqa: E402


def boxed(codestream: bytes) -> bytes:
    """A minimal JP2 wrapper around a codestream."""
    signature = struct.pack(">I", 12) + b"jP  " + b"\r\n\x87\n"
    brand = struct.pack(">I", 20) + b"ftyp" + b"jp2 " + b"\0\0\0\0" + b"jp2 "
    contiguous = struct.pack(">I", 8 + len(codestream)) + b"jp2c" + codestream
    return signature + brand + contiguous


def codestream(
    width: int = 8,
    height: int = 8,
    components: int = 1,
    tile: int = 8,
    tile_parts: tuple[int, ...] = (1,),
    declared_parts: int | None = None,
) -> bytes:
    """A codestream shaped like a real one, carrying no image data.

    It is never decoded: it exists so the header readers and the tile-part
    repair can be checked against fields whose values this function chose.
    """
    siz = struct.pack(
        ">HIIIIIIIIH",
        0,
        width,
        height,
        0,
        0,
        tile,
        tile,
        0,
        0,
        components,
    ) + bytes([7, 1, 1]) * components
    cod = bytes([0, 1]) + struct.pack(">H", 3) + bytes([1, 5, 4, 4, 0, 1])
    qcd = bytes([0x40]) + bytes([9 << 3]) * 16
    out = b"\xff\x4f"
    out += b"\xff\x51" + struct.pack(">H", len(siz) + 2) + siz
    out += b"\xff\x52" + struct.pack(">H", len(cod) + 2) + cod
    out += b"\xff\x5c" + struct.pack(">H", len(qcd) + 2) + qcd
    for tile_index, parts in enumerate(tile_parts):
        for part in range(parts):
            body = b"\x00" * 4
            declared = parts if declared_parts is None else declared_parts
            sot = struct.pack(">HIBB", tile_index, 12 + 2 + len(body), part, declared)
            out += b"\xff\x90" + struct.pack(">H", len(sot) + 2) + sot
            out += b"\xff\x93" + body
    out += b"\xff\xd9"
    return out


class Inventory(unittest.TestCase):
    def test_the_reader_reports_the_fields_that_were_written(self):
        report = inventory.read(codestream(width=40, height=24, components=3, tile=16))
        self.assertEqual(report["siz"]["size"], (40, 24))
        self.assertEqual(report["siz"]["tile"], (16, 16))
        self.assertEqual(len(report["siz"]["components"]), 3)
        self.assertEqual(report["siz"]["components"][0]["depth"], 8)
        self.assertFalse(report["siz"]["components"][0]["signed"])
        self.assertEqual(report["cod"]["progression"], "RLCP")
        self.assertEqual(report["cod"]["layers"], 3)
        self.assertTrue(report["cod"]["mct"])
        self.assertEqual(report["cod"]["levels"], 5)
        self.assertEqual(report["cod"]["code_block"], (64, 64))
        self.assertEqual(report["cod"]["transform"], "5/3 reversible")
        self.assertEqual(report["qcd"]["style"], "none (reversible)")
        self.assertEqual(report["qcd"]["guard_bits"], 2)

    def test_a_jp2_wrapper_is_unwrapped_to_the_same_codestream(self):
        raw = codestream()
        self.assertEqual(inventory.codestream(boxed(raw)), raw)
        self.assertEqual(oracle.unwrap(boxed(raw)), raw)

    def test_a_bare_codestream_is_left_alone(self):
        raw = codestream()
        self.assertEqual(oracle.unwrap(raw), raw)


class TilePartRepair(unittest.TestCase):
    def test_a_correct_count_is_not_touched(self):
        raw = codestream(tile_parts=(3,))
        repaired, changed = oracle.repair_tile_part_counts(raw)
        self.assertFalse(changed)
        self.assertEqual(repaired, raw)

    def test_a_wrong_count_is_corrected_and_nothing_else_changes(self):
        raw = codestream(tile_parts=(3,), declared_parts=5)
        repaired, changed = oracle.repair_tile_part_counts(raw)
        self.assertTrue(changed)
        self.assertEqual(len(repaired), len(raw))
        differing = [i for i, (a, b) in enumerate(zip(raw, repaired)) if a != b]
        # One byte per tile-part, and each is the count field of its own SOT.
        self.assertEqual(len(differing), 3)
        for index in differing:
            self.assertEqual(raw[index], 5)
            self.assertEqual(repaired[index], 3)

    def test_every_tile_is_counted_separately(self):
        raw = codestream(tile=4, tile_parts=(2, 1, 3, 1), declared_parts=9)
        repaired, _ = oracle.repair_tile_part_counts(raw)
        counts = []
        for offset in oracle.tile_part_offsets(repaired):
            counts.append((struct.unpack(">H", repaired[offset + 4 : offset + 6])[0],
                           repaired[offset + 11]))
        self.assertEqual(
            counts,
            [(0, 2), (0, 2), (1, 1), (2, 3), (2, 3), (2, 3), (3, 1)],
        )


class Comparison(unittest.TestCase):
    """The negative controls: the comparison must be able to disagree."""

    @classmethod
    def setUpClass(cls):
        try:
            from PIL import Image  # noqa: F401
        except ImportError:  # pragma: no cover -- the probe says so and stops
            raise unittest.SkipTest("Pillow is not installed")
        if not oracle.DECODER.exists():
            raise unittest.SkipTest(f"{oracle.DECODER} is not built")
        cls.directory = Path(tempfile.mkdtemp(prefix="jpeg2000-controls-"))
        cls.image = cls.directory / "control.j2k"
        from PIL import Image

        picture = Image.new("L", (32, 24))
        pixels = picture.load()
        for y in range(24):
            for x in range(32):
                pixels[x, y] = (x * 8 + y * 3) % 256
        picture.save(cls.image, format="JPEG2000", irreversible=False)

    def compare_with(self, script: str) -> str:
        """Runs the comparison against a stand-in decoder of our choosing."""
        stand_in = self.directory / "stand-in"
        stand_in.write_text(script)
        stand_in.chmod(0o755)
        real = oracle.DECODER
        oracle.DECODER = stand_in
        try:
            verdict, _ = oracle.compare(self.image, 3, self.directory)
        finally:
            oracle.DECODER = real
        return verdict

    def test_the_real_decoder_agrees_with_the_reference(self):
        verdict, line = oracle.compare(self.image, 3, self.directory)
        self.assertEqual(verdict, "exact", line)

    def test_one_wrong_byte_is_reported_as_a_difference(self):
        verdict = self.compare_with(
            "#!/usr/bin/env python3\n"
            "import subprocess, sys, pathlib\n"
            f"subprocess.run(['{oracle.DECODER}'] + sys.argv[1:], check=True)\n"
            "p = pathlib.Path(sys.argv[-1]); b = bytearray(p.read_bytes())\n"
            "b[10] ^= 0xFF\n"
            "p.write_bytes(bytes(b))\n"
        )
        self.assertEqual(verdict, "differs")

    def test_a_short_answer_is_reported_as_a_shape_mismatch(self):
        verdict = self.compare_with(
            "#!/usr/bin/env python3\n"
            "import sys, pathlib\n"
            "pathlib.Path(sys.argv[-1]).write_bytes(b'too short')\n"
        )
        self.assertEqual(verdict, "shape")

    def test_a_decoder_that_refuses_is_reported_as_a_refusal(self):
        verdict = self.compare_with("#!/usr/bin/env python3\nimport sys; sys.exit(1)\n")
        self.assertEqual(verdict, "refused")

    def test_a_tolerance_is_not_a_licence_to_differ_by_more(self):
        stand_in = self.directory / "off-by-five"
        stand_in.write_text(
            "#!/usr/bin/env python3\n"
            "import subprocess, sys, pathlib\n"
            f"subprocess.run(['{oracle.DECODER}'] + sys.argv[1:], check=True)\n"
            "p = pathlib.Path(sys.argv[-1])\n"
            "p.write_bytes(bytes(min(b + 5, 255) for b in p.read_bytes()))\n"
        )
        stand_in.chmod(0o755)
        real = oracle.DECODER
        oracle.DECODER = stand_in
        try:
            within, _ = oracle.compare(self.image, 5, self.directory)
            beyond, _ = oracle.compare(self.image, 4, self.directory)
        finally:
            oracle.DECODER = real
        self.assertEqual(within, "within-tolerance")
        self.assertEqual(beyond, "differs")


if __name__ == "__main__":
    unittest.main(verbosity=2)
