# Hippo Transcribe

**Private by design. Offline by default.**

Local, offline speaker diarization and transcription. A single native Rust
executable provides both the desktop GUI (iced) and a command-line mode
(`--cli`). No Python. Network access is limited to downloading models.

**The name comes from HIPAA.** Audio is processed on your machine, and
transcripts stay there.

No cloud · No accounts · No telemetry · Audio and transcripts stay on this device.

- **Diarization** — [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx):
  `pyannote/segmentation-3.0` ONNX + a `wespeaker` speaker-embedding ONNX, with
  agglomerative clustering.
- **Transcription** — [whisper.cpp](https://github.com/ggerganov/whisper.cpp)
  (GGML models) via `whisper-rs`.
- **Audio** — `symphonia` decode + `rubato` resample to 16 kHz mono.

The pipeline diarizes the audio, cleans up the speaker turns, then transcribes
each speaker turn independently; the speaker label comes from the diarization
turn, so transcription timing never affects speaker assignment. The user picks
the speaker count with `--num-speakers N` (default 2); there is no auto-detect.

## Workspace

- `PRIVACY.md` — data flows, storage locations, network use and retention.
- `docs/brand.md` — approved privacy messaging and banned claims.
- `core/` — `hippo-transcribe-core` library: audio, diarization, ASR, model/cache paths,
  pipeline, CLI.
- `gui/` — `hippo-transcribe-gui` binary: the iced GUI and the `--cli` entry point.

## Build

Requires a **Rust toolchain**, **CMake**, and a C/C++ compiler (clang on Linux,
MSVC on Windows). `whisper.cpp` is built from source; on Linux `sherpa-onnx` is
too, while Windows loads prebuilt sherpa-onnx DLLs at runtime.

```bash
cargo build --release            # target/release/hippo-transcribe
```

- On Linux x86_64 the non-PIC relocation flag needed for the static sherpa
  libraries is already set in `.cargo/config.toml`.

### Windows

Run the build script from a normal PowerShell prompt:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-windows.ps1
```

It enters the Visual Studio developer environment, configures the static CRT
for the executable, downloads the sherpa-onnx shared DLLs, runs the release
tests, builds, checks the executable's dependencies, stages the bundled model
set from `models.lock.json`, and writes a release folder and zip under `dist\`. Use
`-SkipModels` to stage only the executable and DLLs.

The app loads `sherpa-onnx-c-api.dll` and `onnxruntime.dll` from beside
`hippo-transcribe.exe` at runtime; no native library is linked into the executable. Only
`whisper.cpp` is compiled locally. The release folder contains the executable,
both DLLs, the deploy scripts, README, LICENSE, and the sample. Set
`HIPPO_TRANSCRIBE_SHERPA_DLL` to an explicit path to override the DLL location.

The sherpa-onnx release is a `.tar.bz2`. Windows' built-in tar has no bzip2
support, so the script uses installed 7-Zip or Git for Windows' `bzip2` when
available. If neither is present, it downloads a small standalone 7-Zip
extractor into `%LOCALAPPDATA%\hippo-transcribe\cache` automatically. Alternatively,
extract the archive manually into
`%LOCALAPPDATA%\hippo-transcribe\cache\sherpa-onnx-v1.12.9-win-x64-shared`.

For the edit/run loop, use the fast iteration profile:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\run-windows.ps1
```

That runs `cargo run --profile release-fast` (no LTO, parallel codegen,
incremental) and stages the DLLs beside the development binary. The same
profile is available as `cargo run-fast` if you are already in a Visual Studio
developer prompt. Use the full build script for the final release.

Prerequisites: Visual Studio 2022 Build Tools with the "Desktop development with
C++" workload, Rust stable MSVC (`rustup default stable-x86_64-pc-windows-msvc`),
and CMake (included with the workload or `winget install Kitware.CMake`). LLVM is
optional: without libclang, whisper uses its bundled bindings (`-SkipBindgen`
forces that). Pass `-SkipTests`, `-SkipBindgen`, `-Diagnose`, `-Clean`, `-NoZip`,
`-Version 0.2.0`, or `-OutDir C:\builds` as needed.

## Models

Models are non-gated and installed into `%LOCALAPPDATA%\hippo-transcribe\models`
(Windows) or `~/.cache/hippo-transcribe/models` (Linux/macOS). Downloading is the only
time the app needs the network; the pipeline itself is fully offline.

Download everything at once (also what `deploy.bat` runs on Windows):

```bash
hippo-transcribe --cli models download --verify
```

If models are missing on first launch, the GUI shows a **Download models** card
and fetches the same files in the background with progress. Downloads resume
from a `.part` file, verify SHA-256 checksums, and can be cancelled from the
card. Readiness is checked against the selected preset, so a preset never needs
a model it does not use.

Every preset uses a multilingual model:

- `ggml-tiny-q5_1.bin` (speed) — bundled with release packages
- `ggml-small-q5_1.bin` (balanced) — downloaded on demand
- `ggml-large-v3-turbo-q5_0.bin` (accuracy) — downloaded on demand
- `sherpa-onnx-pyannote-segmentation-3-0/model.onnx`
- `wespeaker_en_voxceleb_resnet34_LM.onnx`

`models.lock.json` is the single source of truth for file names, sizes,
SHA-256 hashes, licenses and mirrors. The application compiles it in, and
`scripts/fetch-models.sh` / `.ps1` stage the same pinned files, so packaging and
runtime downloads cannot disagree.

Downloads try, in order: `HIPPO_TRANSCRIBE_MODEL_MIRROR` (an extra base URL that
serves `<file>`), the public GitLab package mirror
(`librewarelive/hippo-transcribe-models`), then the pinned upstream Hugging Face
/ k2-fsa URLs as fallbacks.

Release packages ship a `models/` directory beside the executable. It is
read-only and used as-is; downloads always go to the writable models directory.

Overrides: `HIPPO_TRANSCRIBE_MODELS_DIR` (root),
`HIPPO_TRANSCRIBE_MODEL_MIRROR` (extra download mirror),
`HIPPO_TRANSCRIBE_SEGMENTATION_MODEL`, `HIPPO_TRANSCRIBE_EMBEDDING_MODEL`,
`HIPPO_TRANSCRIBE_WHISPER_MODEL`. Diarization is cached under
`HIPPO_TRANSCRIBE_CACHE_DIR` (or the platform cache dir) and reused on re-runs;
the cache key includes the model set version. The legacy `TRANSIEL_*` names are
still read as a fallback, and data under the old `transiel` directories is
adopted on first launch.

Report what is present (exit non-zero if anything is missing or has the wrong
size):

```bash
hippo-transcribe --cli models
```

Print model attribution (names, hashes, licenses, sources):

```bash
hippo-transcribe --cli models notice
```

Stage the bundled model set for packaging (verified, cached under
`vendor/models/`, git-ignored):

```bash
scripts/fetch-models.sh            # bundled set only
scripts/fetch-models.sh --all      # include the on-demand accuracy model
scripts/fetch-models.sh --verify-only
```

## CLI

```bash
hippo-transcribe --cli audio.mp3                         # default preset (speed)
hippo-transcribe --cli audio.mp3 --preset balanced
hippo-transcribe --cli audio.mp3 --preset accuracy
hippo-transcribe --cli a.mp3 b.mp3 --output-dir out/     # batch
hippo-transcribe --cli audio.mp3 --no-timestamps
hippo-transcribe --cli audio.mp3 --language es           # or auto
hippo-transcribe --cli audio.mp3 --progress-json         # line-delimited JSON events
hippo-transcribe --cli probe audio.mp3 120               # duration + RMS waveform
```

Presets: `speed` (whisper `tiny`, multilingual), `balanced` (whisper `small`,
multilingual) and `accuracy` (whisper `large-v3-turbo`).

Options: `--preset speed|balanced|accuracy`, `--language <code>|auto`,
`--num-speakers N`, `--output-dir`, `--no-timestamps`,
`--no-cache`, `--progress-json`. Every preset accepts any supported language;
the language auto-detects by default and `--language <code>` pins one.

Output: `<audio-stem>-<preset>-transcription.txt` (`[hh:mm:ss] SPEAKER N: text`),
written next to the audio or into `--output-dir`.

## Acceleration

whisper.cpp uses the fastest backend the build provides:

- **macOS** — Metal on every build. `--features coreml` additionally offloads
  the encoder to Core ML, which requires a converted `*-encoder.mlmodelc` beside
  the model; without one Metal is used.
- **Windows/Linux** — Vulkan + OpenMP when built with `--features gpu`. The
  Windows release script enables it by default (`-Gpu:$false` for a CPU-only
  build) and needs the Vulkan SDK installed.
- **CPU fallback** — if GPU context creation fails, the run continues on the CPU
  and the console reports why. `HIPPO_TRANSCRIBE_ACCEL=cpu` forces CPU.

Every preset routes internally: one speaker skips diarization entirely and
transcribes in one pass; two or more speakers run diarization and then
transcribe each turn. A single-speaker run never loads or downloads the
diarization models, whatever the preset.

Each file logs a `timing ...` line (decode, diarize, ASR, write, realtime
factor). `scripts/benchmark.sh [--preset P] [--speakers N] <audio>...` runs that
over a corpus for backend and preset comparisons.

## Free and Pro

The free tier transcribes with the speed model, shows the transcript window,
and lets you edit, copy and save; playback and search stay available. Pro adds
the balanced and accuracy models, speaker diarization (speaker labels), batch
transcription, and SRT/WebVTT/JSON export.

Pro is a one-time purchase on both stores. A 7-day trial is available once per
install. Entitlement state lives in `license.json` beside the settings and is
never used to delete transcripts or queue items; a lapsed license simply falls
back to the free tier. Store receipt checks survive 30 days offline, then
re-verify.

`HIPPO_TRANSCRIBE_TIER=pro` (or `free`) forces the tier for development. The
store clients are not built yet, so packaged builds report no store and rely on
the trial.

## GUI

Run `hippo-transcribe` with no arguments. Add files/folders to the queue, or drag audio
files and folders anywhere into the window, then choose a preset and speaker
count and press Transcribe. The header theme button cycles light, dark and
system themes. Files added (or imported) while a run is active join the queue
and are processed in a follow-up pass automatically. The gear button (or
`Ctrl+,`) opens Settings for theme, transcript text size, preset, speaker count
(including auto-detect), language, playback speed, follow mode, notifications,
save location, and model storage.

The Export picker writes the current transcript as plain text, SRT, WebVTT, or
JSON. Failed queue items can be retried, and pending items can be reordered.
Keyboard: Up/Down moves through the transcript, Alt+Enter edits the active line,
Space toggles playback, and Left/Right seek ±5 seconds while no text field has
focus.

Transcript search supports a speaker filter, match case (`Aa`) and whole-word
(`W`) matching. Right-click a line to edit it; the editor can copy the line,
merge it with the next line, or split it at the playhead. Line deletion asks for
confirmation. Saves are written atomically and keep a
`<transcript>.hippo-transcribe.bak` backup beside the source file.

Completed rows stay in the queue: click one to revisit that file's transcript,
speaker timeline, waveform and playback. Renaming speakers or saving writes back
to that file's `.txt`. `Import → Transcript` adds an existing transcript as a
completed item (a same-stem audio file beside it is attached automatically);
`Import → Audio` queues a file for transcription.

The queue (statuses, output paths and per-file statistics) is restored on
restart; timeline and waveform data is re-derived on demand from the
diarization cache.

## Deploy (Windows)

`deploy.bat` (shipped beside `hippo-transcribe.exe`) installs the exe to
`%LOCALAPPDATA%\Programs\hippo-transcribe`, creates a Start Menu shortcut named
`Hippo Transcribe`, adds it to the user `PATH`, and downloads the models with the
built-in `curl.exe` (progress shows in the console). Then open a new terminal and
run `hippo-transcribe`. `undeploy.bat` removes the install and shortcut
(`/keepmodels` keeps the models).

## Privacy

Audio, transcripts, diarization caches, models and settings stay on the machine
that runs Hippo Transcribe. There is no cloud service, no account, no telemetry
and no analytics. The only network access is the one-time model download (and
optional checksum-verified mirror overrides).

See [`PRIVACY.md`](PRIVACY.md) for the full data-flow and retention details.

## Licenses

Hippo Transcribe itself is free software, licensed under the GNU General Public
License v3.0. Copyright (C) 2026 Sean Wu. It is provided without warranty;
see LICENSE for details.

- `pyannote/segmentation-3.0` weights: MIT (via the sherpa-onnx ONNX conversion).
- `wespeaker` speaker-embedding weights: CC BY 4.0 (attribution required).
- Whisper GGML models: MIT.
- The Windows bundle redistributes **sherpa-onnx** (Apache-2.0) and **ONNX Runtime** (MIT) DLLs.
- The GUI bundles **Bootstrap Icons** (`gui/assets/bootstrap-icons.ttf`), MIT.
- Other components retain their own licenses (MIT/Apache/etc.).
