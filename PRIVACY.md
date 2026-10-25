# Privacy

Hippo Transcribe processes recordings on the machine where it runs. This
document describes exactly what the application stores and what it sends over
the network.

## What is processed locally

- Audio decoding, diarization and transcription run on this device.
- Audio files are read from the paths you select. They are never uploaded.
- Transcripts are written to the output location you choose (by default, beside
  the source audio).
- The speaker timeline, waveform and per-file statistics are derived locally.

## What is stored

- **Models** — downloaded once into the model directory
  (`%LOCALAPPDATA%\hippo-transcribe\models` on Windows,
  `~/.cache/hippo-transcribe/models` on Linux/macOS).
- **Diarization cache** — optional local cache keyed by audio content and
  diarization settings.
- **Settings and queue** — window layout, presets, save location, queue state and
  transcript paths, stored under the platform configuration directory.
- **Outputs** — the `.txt` transcript and timestamped variant you generate, plus
  the `.hippo-transcribe.bak` backup written when a transcript is edited.

Nothing is stored in a remote account because there are no accounts.

## Network use

The only outbound traffic is model download:

- Model files are fetched from the pinned upstream mirrors, or from a mirror you
  configure with `HIPPO_TRANSCRIBE_MODEL_MIRROR`.
- Downloads resume from a local `.part` file and are verified by SHA-256.
- No audio, transcript, filename, path, usage event or identifier is sent with
  those requests.

After models are installed, the pipeline has no network dependency.

## Telemetry and accounts

There is no telemetry, analytics, crash reporting, advertising SDK, license
check or user account. Hippo Transcribe does not assign a device or user
identifier.

## Retention and deletion

Data remains until you delete it. You can:

- Delete transcript files and their `.hippo-transcribe.bak` backups where you
  saved them.
- Clear partial model downloads and remove model files from the model folder.
- Remove the configuration directory to reset settings and the queue.
- Uninstall the application; model and cache directories are not shared with
  other software.

## Security notes

Hippo Transcribe does not encrypt files at rest beyond the protection provided by
your operating system account and disk. For protected health information, use
full-disk encryption, OS account controls and your organization's retention
policy.

## Compliance

Hippo Transcribe is not a certified or accredited product, and this document is
not a Business Associate Agreement. HIPAA obligations apply to covered entities
and their business associates; assess Hippo Transcribe as part of your own
security and compliance program.
