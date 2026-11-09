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
  `~/.cache/hippo-transcribe/models` on Linux/macOS). The 1.1 GB summary model
  is fetched only when someone asks for a summary in a report.
- **Diarization cache** — optional local cache keyed by audio content and
  diarization settings.
- **Settings and queue** — window layout, presets, save location, queue state and
  transcript paths, stored under the platform configuration directory.
- **Outputs** — the transcript you generate, its `.json` sidecar holding speaker
  ids, line bounds and confidences, any export (plain text, SubRip, WebVTT,
  Markdown, CSV or Word) and any report (PDF, Word or plain text) you save, plus
  the `.hippo-transcribe.bak` backup written when a transcript is edited. While a
  run is in progress its lines are parked in a hidden
  `.<audio>.hippo-transcribe.partial` file beside the audio; it is removed when
  the file finishes.
- **Custom vocabulary** — the terms and replacements you enter, stored as
  `vocabulary.json` beside the settings. It holds only what you typed.
- **Summaries** are generated on this machine from the transcript text. The
  text is not uploaded, and the model that writes it is the same kind of local
  file as the transcription models.
- **Microphone recordings** — only when you press Record. Capture happens on
  the machine and the WAV is written where your transcripts go; nothing is sent
  anywhere.

- **Logs** — the last five runs write a local log (`%LOCALAPPDATA%\hippo-transcribe\logs`
  on Windows) recording app events: the version, which model was used, whether the
  GPU or CPU backend was chosen, counts of files processed, and error messages.
  Crashes add a report with a stack trace. Logs never contain audio, transcript
  text or file paths beyond the counts, and are never sent anywhere; deleting them
  is safe.

Nothing is stored in a remote account: there is no account.

## Network use

Hippo Transcribe reaches the network for one reason only: obtaining models.

### Model download

- Model files are fetched from the pinned upstream mirrors, or from a mirror you
  configure with `HIPPO_TRANSCRIBE_MODEL_MIRROR`.
- Downloads resume from a local `.part` file and are verified by SHA-256.
- No audio, transcript, filename, path, usage event or identifier is sent with
  those requests.

After models are installed, transcription itself has no network dependency.

## Telemetry and accounts

There is no telemetry, no analytics, no crash reporting, no advertising SDK and
no Hippo Transcribe account. The application does not assign a device or user
identifier and does not record which features you use.

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
