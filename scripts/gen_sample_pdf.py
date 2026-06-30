#!/usr/bin/env python3
"""Generate a minimal 1-page PDF with some text, print it as base64.
Used to seed the mock backend so the PDF viewer is demoable in a browser."""
import base64
import zlib  # noqa: F401 (kept for parity; not needed for uncompressed stream)


def pdf_bytes() -> bytes:
    text = (
        "BT /F1 20 Tf 72 720 Td (Tine PDF viewer) Tj ET\n"
        "BT /F1 13 Tf 72 690 Td (Select this text to create a highlight.) Tj ET\n"
        "BT /F1 13 Tf 72 668 Td (Highlights persist to assets/<key>.edn + an hls__ page.) Tj ET\n"
    )
    objs = []
    objs.append(b"<< /Type /Catalog /Pages 2 0 R >>")
    objs.append(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
    objs.append(
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
    )
    stream = text.encode("latin-1")
    objs.append(b"<< /Length %d >>\nstream\n" % len(stream) + stream + b"\nendstream")
    objs.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")

    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for i, body in enumerate(objs, start=1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % i + body + b"\nendobj\n"
    xref_pos = len(out)
    out += b"xref\n0 %d\n" % (len(objs) + 1)
    out += b"0000000000 65535 f \n"
    for off in offsets:
        out += b"%010d 00000 n \n" % off
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\n" % (len(objs) + 1)
    out += b"startxref\n%d\n%%%%EOF" % xref_pos
    return bytes(out)


if __name__ == "__main__":
    print(base64.b64encode(pdf_bytes()).decode("ascii"))
