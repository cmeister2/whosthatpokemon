# CPU CLIP backend

Run these commands from the repository root.

Build with `--features clip` and supply a compatible vision ONNX model plus an
[ONNX Runtime](https://github.com/microsoft/onnxruntime/releases) shared library
supporting API 24 (ONNX Runtime 1.24 or newer).

The following Linux x86-64 setup uses a compatible
[quantized CLIP vision export](https://huggingface.co/Xenova/clip-vit-base-patch32/blob/6ef1ebc8b0766a7a8d11b146462c99cdf74dd22d/onnx/vision_model_quantized.onnx)
and the [official CPU runtime](https://github.com/microsoft/onnxruntime/releases/tag/v1.31.0).
These are explicit downloads; each checksum verifies the selected artifact.

```sh
mkdir -p data/models data/runtime
curl -fL \
  'https://huggingface.co/Xenova/clip-vit-base-patch32/resolve/6ef1ebc8b0766a7a8d11b146462c99cdf74dd22d/onnx/vision_model_quantized.onnx' \
  -o data/models/vision_model.onnx
printf '%s  %s\n' \
  '583fd1110a514667812fee7d684952aaf82a99b959760c8d7dca7e0ab9839299' \
  'data/models/vision_model.onnx' | sha256sum --check

curl -fL \
  'https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/onnxruntime-linux-x64-1.31.0.tgz' \
  -o data/runtime/onnxruntime-linux-x64-1.31.0.tgz
printf '%s  %s\n' \
  'cc5c72baf5ae5c8238a6841f0897227be2d02826b9cf98eaf02fdefaeeb03a57' \
  'data/runtime/onnxruntime-linux-x64-1.31.0.tgz' | sha256sum --check
tar -xzf data/runtime/onnxruntime-linux-x64-1.31.0.tgz -C data/runtime
```

Build an index and scan with those files:

```sh
cargo run --release --features clip -- mtg index \
  --backend clip --model data/models/vision_model.onnx \
  --runtime data/runtime/onnxruntime-linux-x64-1.31.0/lib/libonnxruntime.so \
  --catalog data/cards.json --images data/references --output data/mtg.clip.json

cargo run --release --features clip -- scan \
  --backend clip --model data/models/vision_model.onnx \
  --runtime data/runtime/onnxruntime-linux-x64-1.31.0/lib/libonnxruntime.so \
  --index data/mtg.clip.json --cropped data/card-a.png
```

For other platforms, select a CPU package from the official runtime releases or
[installation instructions](https://onnxruntime.ai/docs/install/) and pass its
shared-library path. The CLI expects `pixel_values` float input
`[1, 3, 224, 224]` and pooled `image_embeds` output `[1, dimensions]`, with CLIP
RGB normalization and bicubic center cropping. Supply a vision export matching
that contract; a text encoder or unpooled transformer output is incompatible.
The library also exposes explicit preprocessing settings. Model files and native
runtimes are never downloaded automatically.

CLIP indexes record the actual model SHA-256, preprocessing, and embedding
dimension; scans require the same model and transform. CLIP's exploratory CLI
defaults are `--min-similarity 0.90 --min-margin 0.04`; they can be overridden and
do not establish calibrated accuracy. CLIP embeddings can also confuse similar
printings. The optional `ort` dependency is pinned to the tested release-candidate
API-24 stack; dependency changes require compatible model/runtime verification.

Score scales and defaults differ between backends. Probability estimates require
independent calibration and evaluation on representative captures.

[Documentation](README.md)
