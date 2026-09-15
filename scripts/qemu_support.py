"""Framebuffer and terminal-state helpers shared by the QEMU integration tests."""

from pathlib import Path
import re
import struct
import zlib


FB_BASE = 0xF5A00000
FB_SIZE = 720 * 1280 * 4
FG = 0xFFEEF2F6
GLYPHS = {
    "S": [15, 16, 16, 14, 1, 1, 30],
    "E": [31, 16, 16, 30, 16, 16, 31],
    "L": [16, 16, 16, 16, 16, 16, 31],
    "2": [14, 17, 1, 2, 4, 8, 31],
    "N": [17, 25, 21, 19, 17, 17, 17],
    "G": [14, 17, 16, 23, 17, 17, 15],
    "0": [14, 17, 19, 21, 25, 17, 14],
    "1": [4, 12, 4, 4, 4, 4, 14],
    "7": [31, 1, 2, 4, 8, 8, 8],
    "F": [31, 16, 16, 30, 16, 16, 16],
    "A": [14, 17, 17, 31, 17, 17, 17],
}


def glyph_at(raw: bytes, row: int, column: int) -> list[int]:
    glyph = []
    for glyph_y in range(7):
        bits = 0
        for glyph_x in range(5):
            x = 16 + column * 12 + glyph_x * 2
            y = 16 + row * 16 + glyph_y * 2
            pixel = struct.unpack_from("<I", raw, (1279 - x) * 2880 + y * 4)[0]
            bits = (bits << 1) | (pixel == FG)
        glyph.append(bits)
    return glyph


def park_offset(registers: str, image: bytes) -> int | None:
    match = re.search(r"PC=([0-9a-fA-F]+)", registers)
    if not match:
        return None
    linked_base = struct.unpack_from("<Q", image, 8)[0]
    offset = int(match.group(1), 16) - linked_base
    for candidate in [offset, offset - 4]:
        if (
            0 <= candidate <= len(image) - 4
            and image[candidate : candidate + 4] == struct.pack("<I", 0xD503205F)
        ):
            return candidate
    return None


def png(raw: bytes, path: Path) -> None:
    lines = bytearray()
    for y in range(720):
        lines.append(0)
        for x in range(1280):
            offset = (1279 - x) * 2880 + y * 4
            lines.extend((raw[offset + 2], raw[offset + 1], raw[offset]))

    def chunk(kind: bytes, data: bytes) -> bytes:
        checksum = zlib.crc32(kind + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", checksum)

    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 1280, 720, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(lines))
        + chunk(b"IEND", b"")
    )
