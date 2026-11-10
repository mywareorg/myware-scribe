# Store listing copy

Paste these into Partner Center > Hippo Transcribe > Store listing. The Store renders plain text only: no markdown, no tables, no emoji. Keep the character counts in mind, they are the limits the form enforces.

## Product title

Hippo Transcribe

## Short description (under 200 characters)

Turn recordings into speaker-labelled transcripts on your own PC. No account, no uploads, no audio leaving the machine.

## Long description

Hippo Transcribe turns interviews, meetings and voice notes into readable, speaker-labelled transcripts without sending anything to a server.

Drop in audio or video and it produces a transcript you can read, search, edit and export. Speech recognition and speaker separation run on your own computer. The models download once on first launch — about 215 MB for the standard set — and after that transcription works with the network disconnected. There is no account to create and no usage data to collect.

It is built for the recordings people actually have: interviews, panel discussions, lectures and calls recorded on a phone. A single-speaker recording is transcribed in one pass; a recording with two or more speakers is split into turns first, so each line carries the right speaker. Language detection is automatic, or you can pin a language.

What you get:
- A queue that keeps its rows after a run, so you can see what was processed. Each transcript lands in History the moment its file finishes, marked unread until you open it.
- Export from the Publish menu as plain text, SubRip (.srt) or WebVTT (.vtt) subtitles, Markdown, CSV or Word — with timestamps you can show or hide.
- One-document reports: the transcript, a participant section with how long each speaker talked, and optionally a summary of the key points, saved as PDF, Word or plain text.
- Inline line editing, speaker renaming, a custom dictionary for names and jargon, and freeform labels for organising transcripts.
- Playback with word-level highlighting, a waveform that doubles as a seek bar, and a playback speed control.
- Recording from the microphone with a two-second countdown, then transcription of the take.
- Light and dark themes, and a desktop notification when the whole queue is done.

The free version covers the whole pipeline: the Balanced model, one speaker, one file per run, unlimited files and audio length, microphone recording, copy, search and the automatic transcript. Pro adds the Speed and Accuracy models, speaker labels, batch runs, transcript editing, playback, GPU acceleration, labels, Favorites, and every export and report from the Publish menu. Pro is a one-time purchase with a 7-day trial; the interface is the same in both, so you can see what it adds before you buy.

Everything stays on the machine: audio files, transcripts, models and logs. The only network use is downloading the models. There is no telemetry and nothing is uploaded.

## Feature bullets

- Full-length transcription on your own machine, unlimited files
- Speaker-labelled transcripts (Pro)
- Export as text, .srt or .vtt subtitles, Markdown, CSV or Word (Pro)
- One-document reports with participants and an optional summary (Pro)
- Batch queue for whole folders; finished rows stay visible (Pro)
- History marks new transcripts unread until you open them
- Inline editing, speaker renaming and a custom dictionary (Pro)
- Word-level karaoke playback and waveform seeking (Pro)
- Microphone recording with a two-second countdown
- Automatic language detection, or pin a language
- Works offline once the models have downloaded (about 215 MB once)
- No account, no sign-in, no telemetry
- Light and dark themes, and a notification when the queue finishes

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
- Disk: about 250 MB for the standard model set, or about 600 MB for the accuracy model; the optional summary model adds about 1.1 GB. Plus room for your transcripts and exports.
- Graphics: not required; this release runs inference on the CPU.

## Support and policy

- Support contact: the address registered in Partner Center.
- Website: optional, leave blank.
- Privacy policy URL: `https://hippo-transcribe-1f7a72.gitlab.io/` (live; serves the generated privacy page) (the `pages` job in `.gitlab-ci.yml` publishes `PRIVACY.md` there on every push to `main`; confirm Pages is enabled for the project).
- Copyright: `(c) 2026 Sean Wu`.

## Restricted capability and certification notes

Paste the justification into Partner Center > Properties > Restricted capabilities when the `runFullTrust` capability is flagged.

Hippo Transcribe is a packaged Win32 desktop application (Windows.FullTrustApplication). Full trust is needed to read the audio and video files the user picks from any folder, write transcripts, exports and reports beside them or to a folder the user chooses, load its native speech recognition and speaker separation DLLs from the package, and keep the downloaded models in its own local application data. The microphone capability is used only while the user is recording, behind a visible countdown and stop control. Audio is never uploaded; all processing is local.

Certification notes to paste into the submission:

- First launch downloads the speech recognition models (about 215 MB for the standard set) with progress shown in the app. Transcription works offline afterwards; with no network the app explains that the models are missing. The short sample shipped in the package, sample-short.mp3, gives a quick pass.
- This is a CPU build. Expect roughly real time: a minute of audio takes about a minute on a modern laptop, longer on an older one.
- The microphone capability is exercised by Record: a two-second countdown, then capture, then a Stop control.
- Pro is unlocked by a one-time Store add-on purchase; a 7-day trial is offered once per install from the paywall. The free tier keeps working after the trial ends. A purchase needs a Store-installed build and a Microsoft account; test with the add-on published and the listing hidden.
- Files are written only to the app's own data folder and to locations the user chooses.

## Screenshot shot list

Captured from the 1.0 build at 2880x1920 (3:2; the Store letterboxes it into the 16:9 frame). The eight files in `packaging/screenshots/` are the listing set, in order of upload:

1. `1.png` — light theme, a finished two-speaker interview in speaker bubbles: the waveform with its speaker legend, the player at 2x, and History with a renamed row and an unread dot.
2. `2.png` — the queue with a file waiting and the Transcription Settings dialog open: preset, speakers and acceleration.
3. `3.png` — the run-complete Summary: audio length, run time and speed, then the per-speaker table, over the queue.
4. `4.png` — dark theme: the same interview in bubbles, with History, its unread dots and the transcript toolbar.
5. `5.png` — dark theme, editing: the bottom line-editor strip with the speaker controls, join/split, undo and redo, and the Done control.
6. `6.png` — Generate Report: timestamps, line merging, participants and the summary switch, with the file type set to PDF.
7. `7.png` — recording: the 2-second countdown ring with Stop while the capture starts.
8. `8.png` — Publish -> Export Transcript with all six formats and their extensions listed over a finished recording.

For any recapture, shoot on the Windows build machine at 100% display scaling with the window maximised and the app installed from the candidate MSIX, keep paths and unrelated files off screen, and do not upscale partial-window captures. The Store needs at least one image and accepts up to ten, each at least 1366x768.

## Before submitting

- [ ] Pass `-Version` so the package reports a four-part version, for example `-Version 1.0.0` renders `1.0.0.0`. Every later submission must be higher.
- [ ] Build without `-Sign`: the Store re-signs. `-Sign` is only for sideloading.
- [ ] Confirm `packaging/identity.json` still matches Partner Center character for character. Current values are `SeanWu.HippoTranscribe`, `CN=6862352A-51D9-40F2-9242-0F769B2F87AB`, display name `Sean Wu`.
- [ ] Create the durable Pro add-on in Partner Center before building: give it the product ID `pro` (the client finds the add-on by that ID, so no Store ID is compiled in), set its price and markets, and submit it with the app.
- [ ] Test the purchase flow once the app and add-on pass certification: keep the listing "not discoverable", install the Store build on the dev machine, then buy, restore, refund and go offline. Make the listing public only after that passes.
- [ ] Complete the IARC questionnaire, the age-rating section and the system requirements block.
- [ ] Upload the `.msix`, then submit for certification.

## Notes on wording

`docs/brand.md` bans absolute offline claims. "Works offline once the models have downloaded" is accurate: the first launch fetches the model files, and transcription itself makes no network requests. Mention the one-time download wherever offline use is described, so the Store listing cannot be read as promising an offline install. Do not use "never phones home", "100% offline" or "HIPAA compliant" anywhere in the listing; the privacy policy says "HIPAA inspired" instead.
