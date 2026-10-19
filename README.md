# transiel

Local, offline speaker diarization and transcription. A single native Rust
executable provides both the desktop GUI (iced) and a command-line mode
(`--cli`). No Python, no runtime network access.

- **Diarization** — [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx):
  `pyannote/segmentation-3.0` ONNX + a `wespeaker` speaker-embedding ONNX, with
  agglomerative clustering.
- **Transcription** — [whisper.cpp](https://github.com/ggerganov/whisper.cpp)
  (GGML models) via `whisper-rs`.
- **Audio** — `symphonia` decode + `rubato` resample to 16 kHz mono.

The pipeline diarizes the audio, cleans up the speaker turns, then transcribes
each speaker turn independently; the speaker label comes from the diarization
turn, so transcription timing never affects speaker assignment. The number of
speakers is fixed with `--num-speakers N` (default 2) or auto-detected with
`--auto-speakers`.

## Workspace

- `core/` — `transiel-core` library: audio, diarization, ASR, model/cache paths,
  pipeline, CLI.
- `gui/` — `transiel-gui` binary: the iced GUI and the `--cli` entry point.

## Build

Requires a **Rust toolchain**, **CMake**, and a C/C++ compiler (clang on Linux,
MSVC on Windows). `whisper.cpp` is built from source; on Linux `sherpa-onnx` is
too, while Windows loads prebuilt sherpa-onnx DLLs at runtime.

```bash
cargo build --release            # target/release/transiel
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
tests, builds, checks the executable's dependencies, and writes a release folder
and zip under `dist\`.

The app loads `sherpa-onnx-c-api.dll` and `onnxruntime.dll` from beside
`transiel.exe` at runtime; no native library is linked into the executable. Only
`whisper.cpp` is compiled locally. The release folder contains the executable,
both DLLs, the deploy scripts, README, LICENSE, and the sample. Set
`TRANSIEL_SHERPA_DLL` to an explicit path to override the DLL location.

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

Models are non-gated and installed into `%LOCALAPPDATA%\transiel\models`
(Windows) or `~/.cache/transiel/models` (Linux/macOS). Downloading is the only
time the app needs the network; the pipeline itself is fully offline.

Download everything at once (also what `deploy.bat` runs on Windows):

```bash
transiel --cli models download --verify
```

Existing files are skipped only when their exact size matches upstream; partial
or corrupt downloads are fetched again. A missing preset model stops the run
before any transcript is written.

- `ggml-large-v3-turbo-q5_0.bin` (accuracy, 574 MB),
  `ggml-base.en-q5_1.bin` (speed)
- `sherpa-onnx-pyannote-segmentation-3-0/model.onnx`
- `wespeaker_en_voxceleb_resnet34_LM.onnx`

Overrides: `TRANSIEL_MODELS_DIR` (root), `TRANSIEL_SEGMENTATION_MODEL`,
`TRANSIEL_EMBEDDING_MODEL`, `TRANSIEL_WHISPER_MODEL`. Diarization is cached
under `TRANSIEL_CACHE_DIR` (or the platform cache dir) and reused on re-runs.

Report what is present (exit non-zero if anything is missing or has the wrong
size):

```bash
transiel --cli models
```

## CLI

```bash
transiel --cli audio.mp3                         # default preset (speed)
transiel --cli audio.mp3 --preset accuracy
transiel --cli a.mp3 b.mp3 --output-dir out/     # batch
transiel --cli audio.mp3 --no-timestamps
transiel --cli audio.mp3 --progress-json         # line-delimited JSON events
transiel --cli probe audio.mp3 120               # duration + RMS waveform
```

Presets: `speed` (whisper `base.en`) and `accuracy` (whisper `large-v3-turbo`).

Options: `--preset speed|accuracy`, `--num-speakers N`, `--auto-speakers`,
`--output-dir`, `--no-timestamps`, `--no-cache`,
`--progress-json`.

Output: `<audio-stem>-<preset>-transcription.txt` (`[hh:mm:ss] SPEAKER N: text`),
written next to the audio or into `--output-dir`.

## GUI

Run `transiel` with no arguments. Add files/folders to the queue, choose a
preset and speaker count, then Transcribe. Files added (or imported) while a run
is active join the queue and are processed in a follow-up pass automatically.

Completed rows stay in the queue: click one to revisit that file's transcript,
speaker timeline, waveform and playback. Renaming speakers or saving writes back
to that file's `.txt`. `Import → Transcription` adds an existing transcript as a
completed item (a same-stem audio file beside it is attached automatically);
`Import → Audio` queues a file for transcription.

The queue (statuses, output paths and per-file statistics) is restored on
restart; timeline and waveform data is re-derived on demand from the
diarization cache.

## Deploy (Windows)

`deploy.bat` (shipped beside `transiel.exe`) installs the exe to
`%LOCALAPPDATA%\Programs\transiel`, adds it to the user `PATH`, and downloads the
models with the built-in `curl.exe` (progress shows in the console). Then open a
new terminal and run `transiel`. `undeploy.bat` removes the install
(`/keepmodels` keeps the models).

## Licenses

- `pyannote/segmentation-3.0` weights: MIT (via the sherpa-onnx ONNX conversion).
- `wespeaker` speaker-embedding weights: CC BY 4.0 (attribution required).
- Whisper GGML models: MIT.
- The Windows bundle redistributes **sherpa-onnx** (Apache-2.0) and **ONNX Runtime** (MIT) DLLs.
- The GUI bundles **Bootstrap Icons** (`gui/assets/bootstrap-icons.ttf`), MIT.
- Other components retain their own licenses (MIT/Apache/etc.).
