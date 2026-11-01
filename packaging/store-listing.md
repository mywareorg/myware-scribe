# Store listing copy

Paste these into Partner Center > Hippo Transcribe > Store listing. The Store
renders plain text only: no markdown, no tables, no emoji. Keep the character
counts in mind, they are the limits the form enforces.

## Product title

    Hippo Transcribe

## Short description (under 200 characters)

    Turn recordings into speaker-labelled transcripts on your own PC. No account, no uploads, no audio leaving the machine.

## Long description

    Hippo Transcribe turns interviews, meetings and voice notes into readable
    transcripts without sending anything to a server.

    Drop in an audio or video file and it produces a transcript labelled by
    speaker, with timestamps you can turn off. Everything runs on your own
    computer: every transcription model ships inside the app, so it works with
    the network disconnected from the first launch. There is no account to
    create and no usage data to opt out of.

    It is built for the recordings people actually have: interviews, panel
    discussions, lectures, calls recorded on a phone. Speech recognition and
    speaker separation run locally on the CPU, so it works on an ordinary
    laptop without a graphics card.

    A single-speaker recording is transcribed in one pass. A recording with two
    or more speakers is split into turns first, then transcribed, so each line
    carries the right speaker.

    What you get:
    - A .txt transcript, with or without timestamps, saved beside the audio or
      in a folder you choose.
    - Speaker labels such as SPEAKER 1 and SPEAKER 2.
    - A queue, so a folder of recordings can be left to run.
    - Progress for each file, and a desktop notification when the batch is done.
    - No telemetry and no network use during transcription. (1.1 adds a Store
      licence check when Pro can actually be purchased.)

    The models are included in the package, so the first run needs no internet
    connection and no additional download. Long recordings take time on a CPU;
    expect roughly the length of the recording again for an hour of audio.

    The free version handles any number of files, one speaker at a time, with
    the Balanced model. Pro adds the Speed and Accuracy models, speaker labels,
    batch runs, transcript editing and playback scrubbing; it arrives in 1.1,
    and until then the 7-day trial inside the app shows what it adds.

## Feature bullets

    - Speaker-labelled transcripts from interviews, meetings and notes
    - Runs offline out of the box; the models ship inside the app
    - No account, no sign-in, no telemetry
    - Automatic language detection, or pin a language
    - Timestamps on or off, per export
    - Batch queue for whole folders of recordings
    - Transcript saved as .txt beside the audio or in a chosen folder
    - Re-runs reuse cached results for unchanged audio
    - Desktop notification when a run finishes
    - Logs stay on the machine, and the folder opens from Settings

## Search terms (seven, one per line)

    offline transcription
    speech to text
    speaker diarization
    interview transcript
    meeting notes
    private transcription
    audio to text

## System requirements

- OS: Windows 10 version 1809 (build 17763) or later, Windows 11.
- Architecture: x64.
- Memory: 4 GB minimum, 8 GB recommended.
- Disk: about 2 GB for models, plus room for transcripts.
- Graphics: not required; this release runs inference on the CPU.

## Support and policy

- Support contact: the address registered in Partner Center.
- Website: optional, leave blank.
- Privacy policy URL: `https://librewarelive.gitlab.io/hippo-transcribe/` (live; serves the generated privacy page)
  (the `pages` job in `.gitlab-ci.yml` publishes `PRIVACY.md` there on every push
  to `main`; confirm Pages is enabled for the project).
- Copyright: `(c) 2026 Sean Wu`.

## Screenshot shot list

Capture on the Windows build machine at 1920x1080, window maximised, no personal
file names in the queue, one shot in the dark theme. The Store needs at least
one image and accepts up to ten, each at least 1366x768.

The eight files in `packaging/screenshots/` are the listing set, in order:

1. `01-drop.png` — a finished transcript with the queue, player and speaker bar.
2. `02-running.png` — a run in progress: stage label, progress, elapsed time.
3. `03-transcript.png` — a finished multi-speaker transcript with labels and timestamps.
4. `04-summary.png` — the completion summary.
5. `05-speakers.png` — speaker labels and renaming.
6. `06-settings.png` — Settings: storage, models and the Diagnostics row.
7. `07-sms.png` — the SMS/text-message output mode.
8. `08-dark.png` — the dark theme.

They were captured as partial window shots, so each was scaled with Lanczos to
clear the 1366x768 minimum (factor `max(1366/width, 768/height)`, aspect
preserved). Replace them with full-screen 1920x1080 captures when convenient.

## Before submitting

- [ ] Pass `-Version` so the package reports a four-part version, for example
      `-Version 1.0.0` renders `1.0.0.0`. Every later submission must be higher.
- [ ] Build without `-Sign`: the Store re-signs. `-Sign` is only for sideloading.
- [ ] Confirm `packaging/identity.json` still matches Partner Center character
      for character. Current values are `SeanWu.HippoTranscribe`,
      `CN=6862352A-51D9-40F2-9242-0F769B2F87AB`, display name `Sean Wu`.
- [ ] Paywall: 1.0 ships as a preview. The Pro surfaces stay visible (header
      chip, locked-feature toasts, the nudge after a Free run) and the dialog
      offers the 7-day trial, but no "Buy Pro" button is shown because there is
      no Store entitlement provider yet; the copy says Pro arrives in 1.1. The
      listing must therefore promise a trial and features, not a purchase — do
      not add a price, a "buy" call to action or a Pro price to the listing
      until 1.1 lands with the durable add-on.
- [ ] Complete the IARC questionnaire, the age-rating section and the system
      requirements block.
- [ ] Upload the `.msix`, then submit for certification.

## Notes on wording

`docs/brand.md` bans absolute offline claims. "Runs offline out of the box" is
accurate now that the models ship in the package: transcription makes no network
requests, and the Store licence check happens only when someone buys or restores
Pro. Do not
use "never phones home", "100% offline" or "HIPAA compliant" anywhere in the
listing; the privacy policy says "HIPAA inspired" instead.
