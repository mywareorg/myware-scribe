# Diarization Improvement Plan

Status: Phase 0 complete; Phase 1 dropped; Phase 4 cache work landed; Phase 3 next.

Goal: improve speaker isolation on long recordings without cloud services,
without gated models, and without changing the local-only architecture.

## What the baseline actually says

Eight hand-corrected references, debug build, balanced preset, `--num-speakers 2`
(full table in `tests/diarization/README.md`):

```text
file                         DER@0.25   DER@1.00   absorb s   short
hour-conversation-short-1      53.89%     48.13%      35.03    11/20
hour-conversation-mid-1        34.97%     26.76%      39.15    27/52
hour-conversation-mid-2        33.45%     25.47%      43.61    25/50
sample-full                    14.36%      6.15%      43.94   77/132
hour-conversation-short-2      13.57%      1.82%       4.67    10/20
sample-long                    10.62%      2.76%      10.48    40/59
sample-end                     10.60%      3.58%       2.38    14/20
sample-short                   10.36%      1.60%       1.26      1/6
```

Four conclusions, each of which changed the plan:

1. **The spread is the finding.** 10% to 54% DER at identical settings. Judging
   the diarizer on `sample-long` understated the problem fivefold.
2. **The worst file is a genuine attribution failure, not boundary noise.**
   `hour-conversation-short-1` stays at 48% at a 1.0 s collar: the hypothesis
   collapses 10.4-80.0 s into one speaker while the reference alternates about
   every 2 s. Rapid turn-taking is not tracked at all.
3. **Absorption is the shared mechanism.** Every file loses one speaker's speech
   to the other — 1.3 s to 44 s. `hour-conversation-short-2` is the exception
   where it is really boundary noise (13.57% -> 1.82% at a 1.0 s collar).
4. **The blockwise gate failed.** On the same 300-600 s window of `sample-long`,
   the full-file pass scores 13.15% / 5.55% with 5.6 s absorption; the same audio
   diarized as a 5-minute clip alone scores 15.89% / 8.26% with 13.7 s. Local
   clustering with no cross-block information is *worse*. An earlier run on a
   less-edited reference showed the opposite, which is exactly why the gate had
   to be re-run after the references were finished.

## Target pipeline

```text
decode
  -> split into overlapping blocks
  -> local diarization per block
  -> one mean embedding per local cluster
  -> two global centroids, assigned cluster-by-cluster
  -> speaker turns
  -> Whisper windows
  -> token-level speaker attribution
```

## Model constraints

Use only ungated, locally runnable models:

- Segmentation: `sherpa-onnx-pyannote-segmentation-3-0/model.onnx`
  via the ungated ONNX mirror.
- Accuracy embedding: `wespeaker_en_voxceleb_CAM++_LM.onnx`.
- Fallback embedding: `wespeaker_en_voxceleb_resnet34_LM.onnx`.
- ASR: existing Whisper GGML models.

Do not use the gated `pyannote/speaker-diarization-3.1` pipeline or cloud
diarization APIs. All model files stay in `models.lock.json` with size,
SHA-256, license, and mirrors.

## Phase 0 - Measurement (done)

Delivered:

- `scripts/diarize-score.py`: `score`, `record`, `compare`, `draft`,
  `from-transcript`, `speakers`.
- DER at every requested collar; 0.25 s and 1.0 s by default.
- Per-block ref-vs-hyp confusion matrix (`--matrices`), which is what exposes
  one speaker being absorbed by the other.
- Confusion bucketed by reference turn length (<1 s, 1-3 s, >3 s).
- `--ref-range` / `--hyp-offset`, so a clip can be scored against a slice of a
  longer reference.
- `speakers --expect N` for the phantom-speaker check.
- `--diarize-only` now applies the pipeline's own `clean_turns` and prints a
  `#` timing line, so a baseline matches the shipped turn geometry.

Mono check: `sample-full` holds a 2.9-minute single-speaker run at 9:31-12:22.
Score the restricted window and assert one label; no separate recording needed.

## Phase 1 - Turn geometry (dropped)

Was going to tighten `min_duration_on` / `min_duration_off` in
`core/src/diarize.rs` and relabel sub-1s turns whose embedding matches both
neighbours. Dropped for three reasons, recorded so it is not reopened:

- The baseline says this is not where the error is. Only 9.4% of confusion sits
  in turns under 1 s (2.98 s of 31.73 s); 63.4% sits in turns over 3 s. Even a
  perfect fix buys about 0.7 DER points, on the noisiest bucket, because the
  reference's turn ends are estimated from ASR line starts.
- `clean_turns` already merges same-speaker gaps up to 1.0 s and drops turns
  under 0.4 s (`core/src/pipeline.rs`), so `min_duration_off = 0.3` is dominated
  by the existing merge, and `min_duration_on = 0.6` only removes short regions
  — the opposite of the short-turn recall it was meant to raise.
- The relabel half needs per-turn embeddings. `core/src/sherpa_dynamic.rs` binds
  no embedding extractor, so it is new FFI plus centroid code, not a tuning
  change.

## Phase 2 - Decidability gate (failed, superseded)

The gate asked whether local diarization beats the full-file pass on the same
audio. It does not: on the 300-600 s window of `sample-long`, the full-file pass
scores 13.15% / 5.55% with 5.6 s absorption, and the same audio diarized as an
independent 5-minute clip scores 15.89% / 8.26% with 13.7 s. Fewer embeddings per
cluster and no cross-block evidence lose more than the block boundary gains.

So independent blocks are not the improvement. Blockwise is implemented
(`4188378`, `69e6a1b`); it chains labels through the overlap, which is exactly
the part that has to be replaced by cluster-mean embedding assignment. All
acceptance is measured against the per-file table in
`tests/diarization/README.md`, including a re-measure of the shipped 300/45
geometry against the finished references.

## Phase 3 - Blockwise diarization (measured, removed)

Built, measured, and deleted. Five-minute blocks with a 20-45 s overlap and a
chained label assignment through the overlap score worse than the single pass on
every file long enough to have more than one block:

```text
file      single @0.25/@1.0     blocks 300/25     blocks 300/45
long         10.62 / 2.76        12.29 / 4.32      21.45 / 15.71
full         14.36 / 6.15            -             30.24 / 23.87
```

`full` also loses 250.6 s of one speaker to the other under 300/45, against
43.9 s in the single pass. The chained assignment is the defect: one wrong link
inside an overlap propagates through every later block. Files of 300 s or less
were unaffected either way, which is why the change looked plausible.

Files, `HIPPO_TRANSCRIBE_DIARIZE_BLOCKS`, the block geometry constants and the
progress composition were removed rather than left behind a switch. The
measurements stay in `tests/diarization/README.md`.

What the phase established, and what replaces it: the failure it was meant to
fix is real (one speaker absorbed for tens of seconds on `mid-1` and `short-1`),
but blocks are the wrong axis. Three further approaches were measured and
rejected - turn-level re-clustering, a dense window track, and three identity
rules for split turns - each documented with numbers. The common cause is model
quality per recording, not the geometry of the pass: the same embedding model
wins on one file and loses by 25 points on another.

## Phase 4 - UI, cache, progress

- (done) `DIARIZER_VERSION` in the cache key, mirroring `tokens-v3`.
  `models::diarizer_id()` only changes when model files change, so Phase 3 would
  otherwise serve stale single-pass turns. Landed in `534c79f`; bump the const
  with any change to the turn map.
- (done) The key mismatch: `gui/src/backend.rs` reads with `cache::key` (file
  bytes) while the pipeline writes with `samples_hash` (decoded samples), so GUI
  cache reads always missed. `e8c1c6f` writes the turns under both keys, since a
  restored queue has the file and no samples.
- (done) `Only one speaker detected` in the transcript header when diarization
  returns a single voice; the lines are unlabelled, which otherwise reads as a
  failure.
- (done) The blockwise path and its geometry are gone, so the cache salt is a
  constant `single` plus the model pair.

## Phase 5 - Performance and Windows validation

- Gate on **diarize wall time < ASR wall time**, not on a fixed 1.10-1.15x
  factor: that factor is not achievable once 2-3 embeddings per turn are added,
  and the overlap schedule only needs the former.
- Measure: end-to-end factor, diarize factor, peak memory (blockwise exists to
  bound the O(n^2) clustering matrix — that is the number to prove), per-block
  DER, speaker-attributed WER.
- Test matrix: CPU-only Windows, GPU with CPU fallback, 2 min / 30 min / 2 h
  files, paths with spaces, offline install, clean MSIX.

## Recommended order

1. (done, opt-in) Split collapsed turns by embedding drift:
   `HIPPO_TRANSCRIBE_DIARIZE_SPLIT=1` takes `hour-conversation-short-1` from
   53.89% to 27.18% and `mid-2` from 33.45% to 28.22%, at the cost of +0.7 and
   +0.2 points on `sample-long`/`sample-full`. Two identity rules were measured
   and rejected; see `tests/diarization/README.md`. Promoting it to default
   needs the two small regressions removed.
2. Windows and performance validation (Phase 5): the Windows matrix, peak
   memory, and a release-build timing run.

The clustering work is closed unless a better speaker model appears. Four
post-processing designs were measured against the eight-file table and each one
traded files rather than improving them; the remaining error is model quality
per recording.

Viterbi smoothing, overlap research, forced alignment, and incremental
attribution are deferred, each with a documented reason above.
