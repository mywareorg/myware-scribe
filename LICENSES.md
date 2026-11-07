# Third-party notices

Hippo Transcribe itself is licensed under the GNU General Public License v3.0
(see `LICENSE`). It uses the components and model weights below. This file
ships inside the Store package and is also shown in **Help → Licences**.

## Model weights

The models are downloaded at runtime; they are not part of the application
source. Each keeps its own licence and attribution.

- **pyannote/segmentation-3.0** (speaker segmentation), via the sherpa-onnx
  ONNX conversion. MIT. Source: `pyannote/segmentation-3.0`,
  https://huggingface.co/pyannote/segmentation-3.0
- **WeSpeaker CAM++** (speaker embeddings), `wespeaker_en_voxceleb_CAM++_LM`,
  published by the sherpa-onnx project. **CC BY 4.0** — attribution: WeSpeaker,
  https://github.com/wenet-e2e/wespeaker, converted by
  https://github.com/k2-fsa/sherpa-onnx
- **Whisper GGML models** (`ggml-tiny-q5_1`, `ggml-small-q5_1`,
  `ggml-large-v3-turbo-q5_0`). MIT. Source: ggerganov/whisper.cpp,
  https://huggingface.co/ggerganov/whisper.cpp
- **Qwen2.5-1.5B-Instruct GGUF** (briefing model, `qwen2.5-1.5b-instruct-q4_k_m`).
  Apache-2.0. Source: Qwen, https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF

## Runtime components

- **whisper.cpp / ggml** — MIT — https://github.com/ggerganov/whisper.cpp
- **llama.cpp / ggml** — MIT — https://github.com/ggml-org/llama.cpp
- **sherpa-onnx** — Apache-2.0 — https://github.com/k2-fsa/sherpa-onnx
- **ONNX Runtime** — MIT — https://github.com/microsoft/onnxruntime
- **libopus** (through `symphonia-adapter-libopus` / `opusic-sys`) — BSD-3-Clause
  — https://opus-codec.org
- **Microsoft Visual C++ runtime** (`vcruntime140.dll`, `vcruntime140_1.dll`,
  `msvcp140.dll`) — redistributed under the Microsoft Visual Studio
  redistributable terms.
- **Bootstrap Icons** — MIT — https://icons.getbootstrap.com

## Rust dependencies

The application is built from the crates declared in `Cargo.toml` and
`Cargo.lock`. Each retains its own licence (MIT, Apache-2.0, MPL-2.0, BSD and
similar, as declared in its package metadata). The source distribution includes
the dependency graph and lockfile; per-crate licence texts are available from
crates.io and the respective projects.

## Attribution notes

CC BY 4.0 (WeSpeaker) requires attribution when the model is redistributed or
used publicly; the line above is that attribution. Apache-2.0 components
(sherpa-onnx, Qwen2.5) require this notice and the licence text, which are
available at the links above. If you redistribute this application, keep this
file beside the executable or reproduce its contents in your own notices.
