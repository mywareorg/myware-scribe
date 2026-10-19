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
MSVC on Windows) — `sherpa-onnx` and `whisper.cpp` are built from source and
statically linked.

```bash
cargo build --release            # target/release/transiel-gui
```

- On Linux x86_64 the non-PIC relocation flag needed for the static sherpa
  libraries is already set in `.cargo/config.toml`.
- On Windows set `SHERPA_STATIC_CRT=1` (and `+crt-static`) for a fully static
  build, then rename the binary to `transiel.exe` for deployment.

## Models

Models are non-gated and installed into `%LOCALAPPDATA%\transiel\models`
(Windows) or `~/.cache/transiel/models` (Linux/macOS). Downloading is the only
time the app needs the network; the pipeline itself is fully offline.

Download everything at once (also what `deploy.bat` runs on Windows):

```bash
transiel --cli models download --verify
```

- `ggml-large-v3-turbo-q5_0.bin` (accuracy), `ggml-base.en-q5_1.bin` (speed)
- `sherpa-onnx-pyannote-segmentation-3-0/model.onnx`
- `wespeaker_en_voxceleb_resnet34_LM.onnx`

Overrides: `TRANSIEL_MODELS_DIR` (root), `TRANSIEL_SEGMENTATION_MODEL`,
`TRANSIEL_EMBEDDING_MODEL`, `TRANSIEL_WHISPER_MODEL`. Diarization is cached
under `TRANSIEL_CACHE_DIR` (or the platform cache dir) and reused on re-runs.

Report what is present (exit non-zero if anything is missing):

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
- The GUI bundles **Bootstrap Icons** (`gui/assets/bootstrap-icons.ttf`), MIT.
- Other components retain their own licenses (MIT/Apache/etc.).
