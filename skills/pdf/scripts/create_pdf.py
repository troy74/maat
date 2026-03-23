#!/usr/bin/env python3

import json
import os
import sys
from pathlib import Path


def main() -> int:
    raw = os.environ.get("MAAT_SKILL_INPUT", "{}")
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError:
        payload = {}

    workspace_dir = Path(os.environ.get("MAAT_WORKSPACE_DIR", ".")).resolve()
    output_path = payload.get("output_path") or "output/pdf/generated.pdf"
    title = payload.get("title") or "Generated PDF"
    content = payload.get("content") or payload.get("request") or ""

    destination = (workspace_dir / output_path).resolve()
    destination.parent.mkdir(parents=True, exist_ok=True)

    try:
        from reportlab.lib.pagesizes import LETTER
        from reportlab.lib.units import inch
        from reportlab.pdfbase.pdfmetrics import stringWidth
        from reportlab.pdfgen import canvas
    except ImportError:
        print(
            json.dumps(
                {
                    "status": "blocked",
                    "reason": "missing_python_dependency",
                    "message": "reportlab is not installed. Install it with: python3 -m pip install reportlab",
                }
            )
        )
        return 1

    pdf = canvas.Canvas(str(destination), pagesize=LETTER)
    width, height = LETTER
    margin = 0.85 * inch
    cursor_y = height - margin

    pdf.setTitle(title)
    pdf.setFont("Helvetica-Bold", 16)
    pdf.drawString(margin, cursor_y, title)
    cursor_y -= 0.45 * inch

    pdf.setFont("Helvetica", 11)
    max_width = width - (2 * margin)
    line_height = 14

    for paragraph in content.splitlines() or [""]:
        lines = wrap_text(paragraph, max_width)
        if not lines:
            lines = [""]
        for line in lines:
            if cursor_y <= margin:
                pdf.showPage()
                pdf.setFont("Helvetica", 11)
                cursor_y = height - margin
            pdf.drawString(margin, cursor_y, line)
            cursor_y -= line_height
        cursor_y -= 4

    pdf.save()

    print(
        json.dumps(
            {
                "status": "created",
                "path": str(destination.relative_to(workspace_dir)),
                "title": title,
            }
        )
    )
    return 0


def wrap_text(text: str, max_width: float) -> list[str]:
    if not text:
        return [""]

    words = text.split()
    if not words:
        return [""]

    lines: list[str] = []
    current = words[0]

    for word in words[1:]:
        candidate = f"{current} {word}"
        if stringWidth(candidate, "Helvetica", 11) <= max_width:
            current = candidate
        else:
            lines.append(current)
            current = word

    lines.append(current)
    return lines


if __name__ == "__main__":
    sys.exit(main())
