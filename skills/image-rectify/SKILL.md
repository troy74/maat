---
name: image-rectify
description: Rectify scanned or photographed document images with the local page-scanner binary and ONNX model. Use when the user wants to straighten, crop, warp-correct, or clean up a page image without an LLM image-edit call.
metadata:
  short-description: Local command skill for document image rectification with page-scanner.
---

# image-rectify

This is a command-mode MAAT skill backed by the local `bin/page-scanner` binary and `models/seg-model.onnx`.

Use it when the user wants a real document/page rectification pass, not a generative visual restyle.

The wrapper script expects:
- an input image path in the tool call `request`
- optionally `output_path`

It runs `page-scanner` with:
- `--model models/seg-model.onnx`
- `--format img`
- `--cleanimg crisp`

and returns the produced output path in stdout.

## Default model

- **Default path:** `models/seg-model.onnx` relative to the **current working directory** when the binary is run.
- **Where to expect it:** Place `seg-model.onnx` in a `models/` directory next to where you run the binary (e.g. `./models/seg-model.onnx`), or run the binary from the project root so `models/seg-model.onnx` resolves.
- **Other models:** Any ONNX model can be used by passing `--model /path/to/model.onnx`; the default is only used when `--model` is omitted.

## CLI parameters

| Option | Default | Description |
|--------|---------|-------------|
| `INPUT` | (required) | Single image (png/jpg) or folder of images |
| `--output`, `-o` | — | Output path for single image (overrides `--outdir` for that file) |
| `--format` | `pdf` | `pdf`, `img`, or `both` |
| `--outdir` | `output` | Output directory |
| `--limit` | `10` | Max images when input is a folder; `0` = unlimited |
| `--model` | `models/seg-model.onnx` | Path to ONNX model |
| `--cleanimg` | `grayscale` | `default`, `original`, `grayscale`, `bw`, `highcontrast`, `crisp`, `sharp` |
| `--ocr` | `none` | `none`, `auto` (try tesseract else no OCR), or `tesseract` |
| `--llm` | off | Call OpenAI with OCR text (needs build with `--features llm` and `OPENAI_API_KEY`) |
| `--debug_bbox` | off | Write `{base}_bbox.png` with detected bbox |
| `--savemask` | off | Write `{base}_mask.png` with detection mask |

## Examples

```bash
page-scanner input.jpg
page-scanner input.jpg -o out.pdf
page-scanner folder/ --limit 50 --format both
page-scanner input.jpg --model /path/to/custom.onnx
page-scanner input.jpg --ocr
page-scanner input.jpg --ocr tesseract
```
