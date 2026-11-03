# Run performance

How a transcription run spends its time, what the pipeline parallelises, and
how to measure a change.

## Stages

Per file: `decode → diarize → transcribe → attribute → write`.

The run emits a timing line per file and one for model load:

```
timing models: whisper X.XXs, startup X.XXs
timing <name> [classic|overlap]: decode X.XXs hash X.XXs prepare X.XXs diarize X.XXs \
  asr X.XXs (busy X.XXs, NN% of N worker(s), N call(s), N cached) write X.XXs \
  total X.XXs (audio X.Xs, X.Xx realtime)
```

- `decode` — time blocked waiting for the audio decode (`(prefetched)` when the
  next file was decoded in the background).
- `hash` — SHA-256 over the decoded samples, used by both caches.
- `prepare` — classic: turn cleanup, speaker map, clip building, waveform;
  overlap: attribution of finished text to speakers.
- `diarize` — sherpa-onnx only, measured inside the diarization thread, so it is
  meaningful when it runs beside transcription.
- `asr` — wall time of the worker phase; `busy` is the sum of worker-seconds
  actually spent inside whisper, so `busy / (asr × workers)` is pool utilisation.
- `write` — the `.txt`, `-ts.txt` and `-ts.json` outputs plus stats.

## Schedules

`HIPPO_TRANSCRIBE_PIPELINE` (legacy `TRANSIEL_PIPELINE`) selects the schedule:

- `classic` — diarize first, then transcribe the resulting speaker clips. Lines
  stream while transcription runs.
- `overlap` — transcribe fixed ~29 s windows while diarization runs, then
  attribute the text to speakers. Lines arrive once diarization completes.

The default is `overlap` on machines with **8 or more physical cores** and
`classic` below that, because the two stages contend for the same cores on small
machines. The decision is logged at start of every run:

```
schedule: overlap (12 physical core(s), 4 ASR worker(s) x 2 requested thread(s))
```

Thread budgets:

- `HIPPO_TRANSCRIBE_ASR_WORKERS` (legacy `TRANSIEL_ASR_WORKERS`) — worker pool size.
- `HIPPO_TRANSCRIBE_ASR_THREADS` (legacy `TRANSIEL_ASR_THREADS`) — threads per call.
- `HIPPO_TRANSCRIBE_DIARIZE_THREADS` (legacy `TRANSIEL_DIARIZE_THREADS`) — sherpa
  threads. In overlap mode the run reserves about a third of the physical cores
  for diarization and gives the rest to the ASR pool.

Other knobs:

- `HIPPO_TRANSCRIBE_BEST_OF` (legacy `TRANSIEL_BEST_OF`) — decoder samples per
  segment. Default 1; raise it to A/B the quality tiers.
- `HIPPO_TRANSCRIBE_PREFETCH_MAX_S` (legacy `TRANSIEL_PREFETCH_MAX_S`) — cap on
  the next file's duration that may be decoded in the background. Default 0 (no
  cap). Prefetch holds two decoded buffers: ~230 MB per hour of 16 kHz f32.

## Memory and failure behaviour

Overlap adds no audio buffers: the windows are slices of the already-decoded
`Arc<[f32]>`, and the per-window results are text only (~100 KB per hour of
speech). Peak memory is therefore the same as a classic run plus the model.

If diarization fails while the overlap schedule is running, the already
transcribed text is kept under a single unlabelled speaker and the error is
reported, instead of discarding the file. Classic runs still abort that file,
matching its previous behaviour.

## Caches

Two caches, both under the config directory, both keyed from a SHA-256 of the
decoded samples:

- Diarization: samples hash + diarizer id + speaker count.
- Segments: samples hash + model + language + sample range + `best_of` + `fast`
  + whether token timestamps were requested + a version tag.

The version tag (`tokens-v3`) is bumped whenever the segmentation of audio into
ASR calls changes, so stale entries cannot be reused with different boundaries.
Bumping it is the correct response to any change in clip or window construction.

## Measuring a change

1. Run one file per preset with the default settings and keep the timing line.
2. Re-run with the change and compare `total` and `x realtime`.
3. For overlap, also diff the transcript against a `classic` run:
   `HIPPO_TRANSCRIBE_PIPELINE=classic`. Attribution can differ where a window
   boundary meets a speaker change; that diff is the acceptance check for the
   schedule.
4. Watch the `busy` figure: a low utilisation with a long `asr` wall time means
   the pool is starved (fewer, longer calls), not that whisper is slow.
