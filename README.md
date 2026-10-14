# transiel

Local, CPU-only speaker diarization and transcription.

Diarization, embedding and transcription are **swappable components**, selected
individually or via a preset (`speed`, `balanced`, `accuracy`). See all choices
with:

```bash
.venv/bin/python scripts/components.py --list
```

- **Diarizers:** DiariZen (`diarizen-large`, `diarizen-base`) or a pyannote
  segmentation model (`pyannote-seg3`).
- **Embeddings:** WeSpeaker ResNet34 (`wespeaker-resnet34`).
- **Transcribers:** faster-whisper models (`whisper-turbo`, `whisper-base-en`).

The pipeline diarizes the audio (forced to exactly two speakers), cleans up the
speaker turns, then transcribes each speaker turn independently. Because the
speaker label comes from the diarization turn, transcription timing never
influences speaker assignment. It writes two outputs:

- `<audio-stem>-transcribe.txt` — human readable `[hh:mm:ss] SPEAKER N: text`
- `<audio-stem>-transcribe.json` — the same lines plus diarization turns,
  components, model paths, timings and run parameters for debugging.

## What is in the repository

- **Code** (including the vendored DiariZen source under `third_party/`) is
  tracked in git.
- **Models** are *not* tracked (`models/` is gitignored) and must be downloaded
  on every machine.
- **Virtual environments** are *not* tracked.

## Requirements

- **Python 3.11** (`python3.11`, or `py -3.11` on Windows). The vendored
  DiariZen pins `torch==2.1.1`, which has no Python 3.12 wheels.
- No system FFmpeg is required (audio is decoded with `soundfile`).
- GPU is optional and vendor-selectable (see Setup). No GPU is needed.

## Setup

```bash
python scripts/bootstrap.py                  # auto-detect CPU/CUDA/ROCm/MPS
python scripts/bootstrap.py --vendor cuda    # or cpu|rocm|mps
```

This creates `.venv` and installs the pinned runtime dependencies plus the
vendored DiariZen source and its bundled pyannote.audio fork. `--vendor` selects
the torch wheel channel (`auto` picks macOS→mps, `nvidia-smi`→cuda,
`/opt/rocm`→rocm, else cpu); override the wheel index explicitly with the
`TORCH_INDEX` environment variable. Set `PYTHON` to point at a specific 3.11
interpreter. On Windows the script uses `py -3.11` by default.

**GPU support:** diarization/embedding use PyTorch and run on NVIDIA (CUDA),
AMD (ROCm) or Apple (MPS). Transcription uses CTranslate2, which only supports
**NVIDIA CUDA** — on other vendors transcription runs on CPU. `transiel`
resolves this automatically (`--device auto`).

## Download models

Models are downloaded into `models/`:

```bash
.venv/bin/python scripts/model-download.py                     # download everything missing
.venv/bin/python scripts/model-download.py --preset accuracy   # only a preset's models
.venv/bin/python scripts/model-download.py --component diarizer:diarizen-base
.venv/bin/python scripts/model-download.py --verify            # check completeness only
.venv/bin/python scripts/model-download.py --refresh           # re-download selected
```

On Windows use `.venv\Scripts\python.exe` instead. One model is gated —
`pyannote/segmentation-3.0` (used by the `speed`/`balanced` presets): accept its
conditions on Hugging Face and set `HF_TOKEN` (or pass `--token`) to download it.
All other models are public.

## Run

```bash
.venv/bin/python scripts/transiel.py                       # audio/sample-long.mp3
.venv/bin/python scripts/transiel.py path/to/audio.mp3     # any audio file
.venv/bin/python scripts/transiel.py --speed audio.mp3     # --balanced / --accuracy too
.venv/bin/python scripts/transiel.py --preset speed audio.mp3
.venv/bin/python scripts/transiel.py --diarizer diarizen-base --transcriber whisper-base-en audio.mp3
```

Presets (default `speed`):

- **speed** — `pyannote-seg3` + `wespeaker-resnet34` + `whisper-base-en`; larger
  segmentation step, greedy decoding, all CPU cores. Needs the gated `segmentation-3.0`.
- **balanced** — `pyannote-seg3` + `wespeaker-resnet34` + `whisper-turbo`.
- **accuracy** — `diarizen-large` + `wespeaker-resnet34` + `whisper-turbo` (token-free).

Override any slot with `--diarizer`, `--embedding`, `--transcriber`. Raw path
overrides: `--whisper-model`, `--diarizen-hub`, `--diarizen-embedding`. Other
options: `--output`, `--device`, `--compute-type`. The number of speakers is fixed at 2.

On CPU, diarization dominates the runtime. Long files are slow because each
speaker turn is transcribed separately.

## GUI

A minimal Rust/iced desktop front-end wraps the CLI (it spawns
`scripts/transiel.py` and reads a JSON progress stream):

```bash
cargo run --release --manifest-path gui/Cargo.toml
```

It provides Select File, Save Location (folder), a Speed/Balanced/Accuracy
preset picker, Start/Cancel, a stage-labelled progress bar, and the transcript
on completion. The CLI must be set up first (`scripts/bootstrap.py` +
`scripts/model-download.py`); the GUI locates the repo via `TRANSIEL_ROOT` or by
searching upward for `scripts/transiel.py`.

## Licenses

- The DiariZen model weights are released under **CC BY-NC 4.0** and are
  **non-commercial**. See `third_party/DiariZen/MODEL_LICENSE`.
- Other components retain their own licenses (MIT/Apache/CC-BY).
