# Hippo Transcribe

**Private by design. Offline by default.**

Local, offline speaker diarization and transcription. A single native Rust
executable provides both the desktop GUI (iced) and a command-line mode
(`--cli`). No Python. Network access is limited to downloading models.

**The name comes from HIPAA.** Audio is processed on your machine, and
transcripts stay there. The design is HIPAA inspired; it is not HIPAA certified,
and no certification is claimed.

No cloud · No accounts · No telemetry · Audio and transcripts stay on this device.

- **Diarization** — [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx):
  `pyannote/segmentation-3.0` ONNX + a `wespeaker` speaker-embedding ONNX, with
  agglomerative clustering.
- **Transcription** — [whisper.cpp](https://github.com/ggerganov/whisper.cpp)
  (GGML models) via `whisper-rs`.
- **Audio** — `symphonia` decode + `rubato` resample to 16 kHz mono.

The pipeline diarizes the audio, groups consecutive turns into Whisper-sized
windows, and uses token timestamps to split each ASR segment at speaker
changes. A detached single-word group falls back to the dominant speaker, which
avoids one-word speaker flips at noisy turn boundaries. The user picks the
speaker count with `--num-speakers N` (default 2); there is no auto-detect.

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

It enters the Visual Studio developer environment, runs the release tests,
builds both executables (`hippo-transcribe` and the briefing worker
`hippo-summarize`), then stages them together with the sherpa-onnx and Visual C++
runtime DLLs and the bundled model set from `models.lock.json`, and writes a
release folder and zip under `dist\`. Use `-SkipModels` to stage only the
executable and DLLs. With `-Msix` the package leaves the models out (the Store
build downloads them on first run); `-BundleModels` packs them in anyway.

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

That builds the briefing worker into the same profile, then runs
`cargo run --profile release-fast` (no LTO, parallel codegen, incremental) and
stages the DLLs beside the development binary. The briefing worker is a
separate package and is not a workspace default member, so a bare
`cargo build` or `cargo run` does not produce it:

```powershell
cargo build --profile release-fast -p hippo-summarize
```

Without `hippo-summarize.exe` beside `hippo-transcribe.exe`, Summarize
Transcript reports the helper as missing. In a Visual Studio developer prompt,
`cargo build-fast` builds both binaries into that profile and `cargo run-fast`
runs the GUI. Use the full build script for the final release. Pass `-Free` or
`-Pro` to force the entitlement for that run.

For the GPU edit/run loop, use `scripts\run-gpu.ps1`. It selects the Vulkan
SDK, Ninja, and the DLL CRT, stages the sherpa/ONNX/OpenMP runtime DLLs, and
runs `cargo run --profile release-fast --features gpu`. It defaults to
`HIPPO_TRANSCRIBE_TIER=pro` so GPU calibration is visible; pass `-Free` to test
the free-tier lock.

Prerequisites: Visual Studio 2022 Build Tools with the "Desktop development with
C++" workload, Rust stable MSVC (`rustup default stable-x86_64-pc-windows-msvc`),
and CMake (included with the workload or `winget install Kitware.CMake`). LLVM is
optional: without libclang, whisper uses its bundled bindings (`-SkipBindgen`
forces that). Pass `-SkipTests`, `-SkipBindgen`, `-Diagnose`, `-Clean`, `-NoZip`,
`-Version 0.2.0`, or `-OutDir C:\builds` as needed. Vulkan builds default to
`%USERPROFILE%\hb` as the target directory: ggml's nested shader project generates
paths that exceed cmake's 250-character object path limit inside a deep checkout,
and cl then fails with `Cannot open compiler generated file: ''`. Pass `-TargetDir`
if you want one somewhere else, but keep it short.

If that build machine is also where you push from, `scripts/install-git-shortcuts.ps1`
adds `ga` (`git add`), `gph` (`git push origin main`), `gpl` (`git pull --rebase origin main`) and
`gcm <message>` (`git commit -m <message>`) to your PowerShell profile. If the
shortcuts do not appear, `scripts/fix-shortcuts.ps1` pulls, installs, reloads and
reports what the profile actually contains to `dist\shortcuts-diagnose.txt`.

## Models

Models are non-gated and installed into `%LOCALAPPDATA%\hippo-transcribe\models`
(Windows) or `~/.cache/hippo-transcribe/models` (Linux/macOS). Downloading models
and confirming a Pro purchase are the only reasons the app uses the network; the
transcription pipeline itself never does.

Download everything at once:

```bash
hippo-transcribe --cli models download --verify
```

If models are missing on first launch, the GUI shows a **Download models** card
and fetches the same files in the background with progress. Downloads resume
from a `.part` file, verify SHA-256 checksums, and can be cancelled from the
card. Readiness is checked against the selected preset, so a preset never needs
a model it does not use.

Every preset uses a multilingual model:

- `ggml-tiny-q5_1.bin` (speed) — the free tier's model; shipped with portable releases, downloaded first on a Store install
- `ggml-small-q5_1.bin` (balanced) — downloaded on demand
- `ggml-large-v3-turbo-q5_0.bin` (accuracy) — downloaded on demand
- `sherpa-onnx-pyannote-segmentation-3-0/model.onnx`
- `wespeaker_en_voxceleb_CAM++_LM.onnx`

The briefing uses a separate model, downloaded the first time someone asks for
one rather than with any preset:

- `qwen2.5-1.5b-instruct-q4_k_m.gguf` — Qwen2.5-1.5B-Instruct, Apache-2.0,
  1.1 GB

`models.lock.json` is the single source of truth for file names, sizes,
SHA-256 hashes, licenses and mirrors. The application compiles it in, and
`scripts/fetch-models.sh` / `.ps1` stage the same pinned files, so packaging and
runtime downloads cannot disagree.

Downloads try, in order: `HIPPO_TRANSCRIBE_MODEL_MIRROR` (an extra base URL that
serves `<file>`), the public GitLab package mirror
(`librewarelive/hippo-transcribe-models`), then the pinned upstream Hugging Face
/ k2-fsa URLs as fallbacks.

Portable release packages ship a `models/` directory beside the executable. It
is read-only and used as-is; downloads always go to the writable models
directory. Store packages leave the models out and download them on first run,
so the package and its updates stay small.

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
hippo-transcribe --cli audio.mp3 --preset speed
hippo-transcribe --cli audio.mp3 --preset accuracy
hippo-transcribe --cli a.mp3 b.mp3 --output-dir out/     # batch
hippo-transcribe --cli audio.mp3 --no-timestamps
hippo-transcribe --cli audio.mp3 --language es           # or auto
hippo-transcribe --cli audio.mp3 --progress-json         # line-delimited JSON events
hippo-transcribe --cli probe audio.mp3 120               # duration + RMS waveform
```

Presets: `speed` (whisper `tiny`, multilingual), `balanced` (whisper `small`,
multilingual) and `accuracy` (whisper `large-v3-turbo`). `speed` is the default
and the free tier's preset; `balanced` and `accuracy` require Pro.

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
- **Windows/Linux** — Vulkan + OpenMP with `--features gpu`. Opt-in: the Windows
  script builds CPU-only unless you pass `-Gpu`, which requires the Vulkan SDK.
  An installed SDK does not switch the backend on by itself. The SDK is only
  needed to compile — ggml loads `vulkan-1.dll` from the driver at run time.
  The Windows GPU build also stages the Visual C++ OpenMP runtime
  (`vcomp140.dll`) beside the executable.
  This path has never been run on real hardware, so treat it as experimental.
- **CPU fallback** — if GPU context creation fails, the run continues on the CPU
  and the console reports why. `HIPPO_TRANSCRIBE_ACCEL=cpu` forces CPU.
- **Acceleration** — Pro chooses CPU or GPU below Preset/Speakers. CPU is
  the `1×` baseline; GPU shows the measured speed multiplier once calibrated.
  The `Now:` line reports the backend actually used by the running pipeline.
  On the first run for a preset, a **GPU Calibration** window offers a short
  CPU-vs-GPU test; testing progress, results, and the choice all stay in that
  window. Closing it keeps CPU for that preset; `↻` retests. Free sees GPU
  locked, and CPU-only builds show it unavailable. A transcription error, or an
  unclean shutdown during a GPU run, returns that preset to CPU automatically.
- **Windows GPU troubleshooting** — run `.\scripts\test-gpu-build.ps1` before a
  build; `-Probe` isolates the ggml shader configure, `-Build` performs the GPU
  build, and `-Artifact` checks the finished executable's runtime DLLs. The
  report is written to `dist\gpu-test.txt`.

Every preset routes internally: one speaker skips diarization entirely and
transcribes in one pass; two or more speakers run diarization and transcribe
short windows that can span several turns, then assign token timestamps back to
the matching turn. A single-speaker run never loads or downloads the diarization
models, whatever the preset.

Each file logs a `timing ...` line (decode, diarize, ASR, write, realtime
factor). Per-clip ASR results are cached by audio hash, model, language and
decode settings, so re-runs and re-opened transcripts skip whisper entirely;
clip edges are silence-trimmed before encoding. `scripts/benchmark.sh [--preset P] [--speakers N] <audio>...` runs that
over a corpus for backend and preset comparisons.

## Free and Pro

The free tier is the full-length pipeline: the Speed model, one speaker, one
file per run, unlimited files and audio length, microphone recording, copy,
search and the automatic `.txt` transcript. Pro adds the Balanced and Accuracy
presets, speaker diarization and labels, batch runs, transcript editing, audio
playback, GPU acceleration, labels, Favorites and every export or report from
the Publish menu.

Pro is a one-time purchase on the Store, with a 7-day trial once per install.
The packaged Windows build asks `Windows.Services.Store` whether the durable
`pro` add-on is owned and starts the purchase from the paywall; portable builds
offer the trial only. Entitlement lives in `license.json` beside the settings and
never deletes transcripts or queue items; a lapsed licence falls back to the free
tier, and a checked entitlement survives 30 days offline before it re-verifies.

`HIPPO_TRANSCRIBE_TIER=pro` (or `free`) forces the tier for development.

## GUI

Run `hippo-transcribe` with no arguments. The File panel on the left holds the
queue and the finished transcripts. Add files/folders from its action row, or
drag audio files and folders anywhere into the window, then choose a preset and
speaker count and press Transcribe. **Record** captures from the microphone
(48 kHz mono WAV, written beside your transcripts or to the save location): a
two-second countdown starts the take, the meter and stop control live in the
transcription pane, and stopping transcribes it immediately with no setup —
the Speed preset, one speaker. Pressing Record while a batch is transcribing stops the batch, leaves the
queued files for later, and records the take on its own. A recording workflow
locks the panel until it finishes. The microphone is chosen in Settings. The Help menu holds the appearance choices (Light,
Dark, System; System follows the desktop and is the default) and the header
theme button cycles them. Files added while a run is active join the queue and
are processed in a follow-up pass automatically. The gear button (or `Ctrl+,`)
opens Settings for transcript text size, preset, speaker count, language,
loudness normalization, playback speed, follow mode, notifications, save
location, and model storage.

**Publish → Generate Report** builds one document from the finished transcript:
the transcript itself, a participants section with each speaker's speaking time,
and, when asked for, a summary of the overview and key points with citations.
The summary step downloads a 1.1 GB model once; it runs on this machine and can
be cancelled while it works, and a finished summary is cached beside the
transcript as `<stem>-briefing.json` so reopening it costs nothing. The report
saves as PDF, Word or plain text through the native save dialog. It is a derived
document, not a record: the transcript stays the source of truth.

The briefing helper runs on the CPU by default. Built with
`cargo build -p hippo-summarize --features vulkan` (or `metal`/`cuda`), set
`HIPPO_BRIEFING_GPU_LAYERS` to the number of layers to offload; a CPU-only build
ignores the variable.

The Export picker (Pro) writes the current transcript as plain text, SRT,
WebVTT, Markdown, CSV, or a Word document; typing a `.json` name still writes
JSON. Subtitle formats use the exact line bounds stored in the
transcript's sidecar; long lines are split at word boundaries when the decoder
produced word times, and a transcript without timestamps is refused rather than
exported at zero. Failed queue items can be retried, and pending items can be
reordered.

Word times are stored in the sidecar. While audio plays, the words already
spoken on the active line are banded and the current word underlined
(View → Karaoke, key `k`; Pro). The Speed preset decodes without
token timestamps, so those transcripts — and any produced before this change —
export line-level cues instead.
Keyboard: Up/Down moves through the transcript, Alt+Enter edits the active line,
Space toggles playback, and Left/Right seek ±5 seconds while no text field has
focus. Editing and playback keys require Pro.

The Edit and Help menus both open Custom Vocabulary, where names, acronyms and
jargon can be listed for the decoder and misheard words can be paired with their
correct spelling. Terms apply to the next run; replacements are applied to the
transcript text afterwards.

Transcript search supports a speaker filter, match case (`Aa`) and whole-word
(`W`) matching. Right-click a line to edit it; the editor can copy the line,
merge it with the next line, or split it at the playhead. Line deletion asks for
confirmation. Saves are written atomically and keep a
`<transcript>.hippo-transcribe.bak` backup beside the source file.

Finished files land in the **History** tab the moment they complete, and the
**Queue** keeps its finished rows visible for the rest of the session.
**Favorites** is a flat starred list (Pro); **Recordings** lists every take,
including a failed one with Retry. History counts the transcripts a run has
produced that have not been opened yet and marks each unread row with a dot;
the Queue counts the work still waiting. History rows are grouped by day (Today, Yesterday, then the date) and
show the file's duration, speaker count and finish time. Each row has a star and
a remove button: a single click opens, a double-click runs the row's safe action
(transcribe, resume or retry, never a retranscribe), and right-click opens a
menu for copy, export, add to queue, reveal and retry. The footer of
History, Recordings and Favorites runs the selected row again (Retry for a
failed recording). Clear queue lives in the Queue tab; Clear history lives in
the History tab and drops the shared finished-item list (History, Favorites and
Recordings) without touching files on disk. Removing an item never deletes the
audio or transcript on disk. Click one to revisit that file's transcript,
speaker timeline, waveform and playback. Renaming speakers or saving writes back
to that file's `.txt`. Adding an audio file with an exact `<name>.txt` sidecar —
beside the audio first, then in the save location — loads it straight away
instead of queueing a run. The app's own
`<name>-<preset>-transcription-<N>speakers.txt` output is not adopted, so
re-adding audio the app has already transcribed queues it for a fresh run.

The queue (statuses and output paths) and the transcript history (finish time,
duration and speaker count) are restored on restart; timeline and waveform data
is re-derived on demand from the diarization cache. A file whose run was
interrupted comes back as **Interrupted** with Resume and per-file Skip in its
row. Resuming reuses the cached ASR clips for that file but recomputes its
diarization, so the speaker timeline is rebuilt from scratch; the button reads
Resume until nothing is left to continue.

## Deploy (Windows)

`deploy.bat` (shipped beside `hippo-transcribe.exe`) installs the exe to
`%LOCALAPPDATA%\Programs\hippo-transcribe`, creates a Start Menu shortcut named
`Hippo Transcribe`, adds it to the user `PATH`, and reports whether the bundled
models are present. The app downloads any missing models on first launch. Then
open a new terminal and run `hippo-transcribe`. `undeploy.bat` removes the
install and shortcut (`/keepmodels` keeps the models).

## Logs

Each run appends to `%LOCALAPPDATA%\hippo-transcribe\logs\hippo.log`, keeping the last
five files at 2 MB each; crashes write `crash-<timestamp>.txt` beside them with a
backtrace and a message box pointing at the folder. Settings → Diagnostics → Open
log folder opens it, so a user reporting a problem always has something to send.
The log records the model, the chosen backend and its fallback reason, and error
messages — never audio, transcript text or file paths.

The policy is also published as a page: `public/index.html` and `public/privacy.html`
are committed so the GitLab Pages job only has to hand over the artifact. Regenerate
them with `python3 scripts/make-privacy-page.py` after editing `PRIVACY.md`.

## Privacy

Audio, transcripts, diarization caches, models and settings stay on the machine
that runs Hippo Transcribe. There is no cloud service, no account, no telemetry
and no analytics. Network access is the one-time model download (with optional
checksum-verified mirror overrides) and, only for a Pro purchase or restore, the
store's entitlement check: the store reports whether the add-on is owned, and
nothing about the audio or the transcript is part of that request.

See [`PRIVACY.md`](PRIVACY.md) for the full data-flow and retention details.

## Licenses

Hippo Transcribe itself is free software, licensed under the GNU Lesser General
Public License v2.1 (LGPL-2.1). Copyright (C) 2026 Sean Wu. It is provided
without warranty; see LICENSE for details.

- `pyannote/segmentation-3.0` weights: MIT (via the sherpa-onnx ONNX conversion).
- `wespeaker` speaker-embedding weights: CC BY 4.0 (attribution required).
- Whisper GGML models: MIT.
- The Windows bundle redistributes **sherpa-onnx** (Apache-2.0) and **ONNX Runtime** (MIT) DLLs, plus the **Microsoft Visual C++ runtime** (`vcruntime140.dll`, `vcruntime140_1.dll`, `msvcp140.dll`), which ship beside the executable so the app runs without the redistributable installed.
- The GUI bundles **Bootstrap Icons** (`gui/assets/bootstrap-icons.ttf`), MIT.
- Other components retain their own licenses (MIT/Apache/etc.).

The full third-party notices are in `LICENSES.md`, shown in **Help → Licenses**
and shipped inside the Store package (the package has no README to fall back
on).
