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

The pipeline diarizes the audio, cleans up the speaker turns, then transcribes
each speaker turn independently. Because the speaker label comes from the
diarization turn, transcription timing never influences speaker assignment. The
number of speakers is fixed with `--num-speakers N` (default 2) or detected with
`--auto-speakers`. It writes one output per file:

- `<audio-stem>-transcript.txt` — human readable `[hh:mm:ss] SPEAKER N: text`

Multiple files can be passed at once; they share a single process, so the models
load once and are reused across the batch.

## What is in the repository

- **Code** (including the vendored DiariZen source under `third_party/`) is
  tracked in git.
- **Models** are *not* tracked (`models/` is gitignored) and must be downloaded
  on every machine.
- **Virtual environments** are *not* tracked.

## Requirements

- **Python 3.11** (`python3.11`, or `py -3.11` on Windows). The vendored
  DiariZen pins `torch==2.1.1`, which has no Python 3.12 wheels.
- A **Rust toolchain** (stable) to build the GUI.
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
.venv/bin/python scripts/transiel.py --output-dir out/ a.mp3 b.mp3 c.mp3  # batch
.venv/bin/python scripts/transiel.py --diarizer diarizen-base --transcriber whisper-base-en audio.mp3
```

Presets (default `speed`):

- **speed** — `pyannote-seg3` + `wespeaker-resnet34` + `whisper-base-en`; larger
  segmentation step, greedy decoding, all CPU cores. Needs the gated `segmentation-3.0`.
- **balanced** — `pyannote-seg3` + `wespeaker-resnet34` + `whisper-turbo`.
- **accuracy** — `diarizen-large` + `wespeaker-resnet34` + `whisper-turbo` (token-free).
  Its batch size and transcription workers are auto-tuned from the available memory.

Hardware auto-tuning: when a tuning value is `0`/absent (batch size, transcription
workers/threads), it is derived from the detected CPU, RAM and device — e.g. the
number of transcription workers is capped so the model (~1.6 GB for `whisper-turbo`)
is not replicated beyond the free memory. Force it for any preset with `--auto-tune`.

Override any slot with `--diarizer`, `--embedding`, `--transcriber`. Raw path
overrides: `--whisper-model`, `--diarizen-hub`, `--diarizen-embedding`. Other
options: `--output` (single file), `--output-dir` (per-file names in a directory),
`--device`, `--compute-type`, `--auto-tune`. The number of speakers can be fixed
with `--num-speakers N` (default 2) or auto-detected with `--auto-speakers`.
Diarization results are cached (`.cache/diarization/`) and reused on re-runs of
the same audio; disable with `--no-cache`. On Windows the cache lives under
`%LOCALAPPDATA%\transiel\cache\`. Override the locations with the
`TRANSIEL_CACHE_DIR` and `TRANSIEL_MODELS_DIR` environment variables.

On CPU, diarization dominates the runtime. Long files are slow because each
speaker turn is transcribed separately.

## Windows distribution (no Python required)

For machines that should not install Python, `scripts/package-windows.py` builds a
self-contained release (Windows build machine only):

```powershell
cargo build --release --manifest-path gui/Cargo.toml
.venv\Scripts\python.exe scripts\model-download.py   # populate models/ first
.venv\Scripts\python.exe scripts\package-windows.py
```

This produces `dist\transiel-windows-x64.zip` containing the GUI exe, `scripts/`,
an embedded Python runtime (`runtime/`) with all dependencies, and the models.
The target machine needs nothing else: extract the zip and run `deploy.bat`, which

- copies the program to `%LOCALAPPDATA%\Programs\transiel`,
- places `models/` beside it (skipped when already present and complete),
- adds the install directory to the **user** `PATH`, and
- is a no-op when the same version is already deployed (use `deploy.bat /force` to
  overwrite).

Then open a new terminal and run `transiel-gui`. Windows SmartScreen may warn on
first launch of the unsigned exe — choose **More info → Run anyway**. Remove the
install with `undeploy.bat` (add `/keepmodels` to retain `models/`).

The packaged app writes its diarization cache to
`%LOCALAPPDATA%\transiel\cache\` and its settings to `%APPDATA%\transiel\`.

## GUI

A Rust/iced desktop front-end wraps the CLI (it spawns
`scripts/transiel.py` and reads a JSON progress stream):

```bash
cargo run --release --manifest-path gui/Cargo.toml
```

Building from source produces the binary at `gui/target/release/transiel-gui`:

```bash
cargo build --release --manifest-path gui/Cargo.toml
```

Features:

- **Queue**: add files (multi-select) or a folder, or drag-and-drop onto the
  window; per-file durations and status. The whole queue runs in a **single
  backend process**, so the models load once and are reused. A completion
  notification and Summary dialog appear when it finishes.
- **Controls**: preset picker (Speed/Balanced/Accuracy), Speakers count (1–5,
  default 2), Save Location, Transcribe/Cancel, and an **Options** panel
  (Confidence tint, Dark theme, Diarization cache, Join Speakers, SMS Mode,
  Speakers, Timestamps). All settings persist across sessions.
- **Transcript**: per-line view with confidence tinting and speaker colours, live
  streaming during a run, Find (match highlight + prev/next), and an always-on
  overlay at the bottom-right with **zoom / Follow / SMS**. Clicking any line
  moves the playhead there and enables Follow. Includes **Copy / Save / Reveal**
  and SMS "chat bubble" mode. After a run a **Summary** dialog appears, then a
  **Rename Speakers** dialog.
- **Timeline & player**: green waveform with a speaker-coloured band and legend,
  scroll-to-zoom, and a black playhead (click/drag to seek); play/pause, time,
  and speed (`.5x/1x/1.5x/2x`) with a speed-synced playhead.
- **Import…** dropdown to load a transcript (`.txt`/`.json`) or an audio file
  (playback only); **Help…** dropdown (About / keyboard shortcuts / console);
  a toggleable **Console** panel showing backend log output.

The CLI must be set up first (`scripts/bootstrap.py` +
`scripts/model-download.py`); the GUI locates the repo via `TRANSIEL_ROOT` or by
searching upward for `scripts/transiel.py`. It runs the pipeline with `.venv`
when present, otherwise the bundled `runtime/` interpreter (packaged Windows
builds).

## Licenses

- The DiariZen model weights are released under **CC BY-NC 4.0** and are
  **non-commercial**. See `third_party/DiariZen/MODEL_LICENSE`.
- The GUI bundles **Bootstrap Icons** (`gui/assets/bootstrap-icons.ttf`), MIT
  licensed, for its UI glyphs.
- Other components retain their own licenses (MIT/Apache/CC-BY).
