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
- No GPU and no system FFmpeg are required. Audio is decoded with `soundfile`.

## Setup

```bash
python scripts/bootstrap.py
```

This creates `.venv` and installs the pinned runtime dependencies plus the
vendored DiariZen source and its bundled pyannote.audio fork. Set the `PYTHON`
environment variable to point at a specific 3.11 interpreter if needed.

On Windows the same command works (the script uses `py -3.11` by default).

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

Presets (default `accuracy`):

- **speed** — `pyannote-seg3` + `wespeaker-resnet34` + `whisper-base-en`; larger
  segmentation step, greedy decoding, all CPU cores. Needs the gated `segmentation-3.0`.
- **balanced** — `pyannote-seg3` + `wespeaker-resnet34` + `whisper-turbo`.
- **accuracy** — `diarizen-large` + `wespeaker-resnet34` + `whisper-turbo` (token-free).

Override any slot with `--diarizer`, `--embedding`, `--transcriber`. Raw path
overrides: `--whisper-model`, `--diarizen-hub`, `--diarizen-embedding`. Other
options: `--output`, `--device`, `--compute-type`. The number of speakers is fixed at 2.

On CPU, diarization dominates the runtime. Long files are slow because each
speaker turn is transcribed separately.

## Licenses

- The DiariZen model weights are released under **CC BY-NC 4.0** and are
  **non-commercial**. See `third_party/DiariZen/MODEL_LICENSE`.
- Other components retain their own licenses (MIT/Apache/CC-BY).
