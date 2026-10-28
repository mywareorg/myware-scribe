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

- **Entitlement** — a Pro purchase writes the tier and the last verified state to
  `license.json` beside the settings. It holds no audio, no transcript and no
  file metadata.

Nothing is stored in a remote account. A Pro purchase is recorded by the store
against the buyer's store account; that record is a purchase, not a transcript.

## Network use

Hippo Transcribe reaches the network in two cases only: obtaining models, and,
in a store build that has been bought or is being restored, asking the store
whether the Pro add-on is owned.

### Model download

- Model files are fetched from the pinned upstream mirrors, or from a mirror you
  configure with `HIPPO_TRANSCRIBE_MODEL_MIRROR`.
- Downloads resume from a local `.part` file and are verified by SHA-256.
- No audio, transcript, filename, path, usage event or identifier is sent with
  those requests.

### Store licensing

- Only a Pro purchase or restore triggers this. The free tier never contacts a
  store: with the models already installed, a free installation generates no
  network traffic at all.
- The request is made by the store client — the Microsoft Store on Windows, the
  App Store on macOS — not by Hippo Transcribe. That client supplies the account,
  device and purchase records it already holds, under the platform operator's
  privacy statement.
- It carries the application and add-on identifiers. It never carries audio,
  transcripts, filenames, file paths, diarization output, timing data or any
  identifier of Hippo Transcribe's own; the application creates none.
- The answer is cached on disk, so Pro keeps working offline. The cached answer is
  re-checked after 30 days, or when the store reports that entitlements changed.

After models are installed, transcription itself has no network dependency.

## Telemetry and accounts

There is no telemetry, no analytics, no crash reporting, no advertising SDK and
no Hippo Transcribe account. The application does not assign a device or user
identifier and does not record which features you use.

Pro buyers are signed in to their platform store account, which is how the store
knows the purchase exists. Hippo Transcribe receives only the resulting
entitlement answer, described above.

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

The design follows the data-minimisation idea behind HIPAA: processing stays on
the device, audio is never uploaded, there is no account holding a transcript,
and nothing is collected for analytics. That makes Hippo Transcribe HIPAA
inspired. It does not make it HIPAA compliant.

Hippo Transcribe is not a certified or accredited product, has not been audited
or assessed by a third party, and this document is not a Business Associate
Agreement. HIPAA obligations apply to covered entities and their business
associates; assess Hippo Transcribe as part of your own security and compliance
program.
