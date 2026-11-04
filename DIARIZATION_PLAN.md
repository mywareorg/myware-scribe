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

## Phase 3 - Blockwise diarization, cluster-level assignment

One phase, replacing the earlier blockwise / centroid / Viterbi trio.

Blocks:

- 5 minutes with 45 s overlap (`HIPPO_TRANSCRIBE_DIARIZE_BLOCKS=1`). Shorter
  overlaps and shorter blocks were measured and were not more stable; see the
  progress note below.
- **Seam ownership by midpoint**: a segment belongs to the block whose centre it
  falls in; drop the duplicate copy before assignment. No seam churn, no nested
  turns at block boundaries.
- Local clusters may be 1..N, but a cluster under 2 s or 10% of the block's
  speech is merged into the nearest local cluster before assignment.

Assignment:

- One mean embedding per **local cluster**, never per segment. This preserves
  the local clustering that was just paid for and removes per-segment flapping
  at the source.
- Two global centroids initialised from the most widely separated cluster means;
  verify the permutation is stable across two independent initialisations.
- Assign each local cluster **wholesale** to a global centroid, with an absolute
  similarity floor and a margin veto against the runner-up.
- Cap influence per **block**, not per segment: a 4.5-minute monologue block
  otherwise dominates the centroid.
- Never recompute a centroid from a cluster whose internal dispersion exceeds
  the inter-centroid distance — that is the re-split check that stops a merged
  cluster from absorbing the other speaker.
- Anchor speaker identity explicitly; do not rely on first-appearance ordering,
  which inverts the whole transcript when one early block is mis-mapped.

Progress:

- Landed behind `HIPPO_TRANSCRIBE_DIARIZE_BLOCKS=1` (default off):
  block splitting with midpoint seam ownership, per-block local diarization,
  small-cluster folding, and a chained label assignment that matches each block
  to the previous one inside their overlap. `HIPPO_TRANSCRIBE_DIARIZE_BLOCK_S`
  and `..._BLOCK_OVERLAP_S` set the geometry; the geometry is part of the
  diarization cache salt.
- Re-measured on `audio/sample-long.mp3` against the finished reference
  (`tests/diarization/sample-long.rttm`, 242 turns), `--num-speakers 2`, debug
  build. The earlier figures in this section were taken against a less-edited
  reference and are superseded:

  ```text
  single pass            10.62% @0.25s   2.76% @1.0s   confusion 18.13s
  blocks 300 / 25        12.29% @0.25s   4.32% @1.0s   confusion 25.93s
  blocks 300 / 45        21.45% @0.25s  15.71% @1.0s   confusion 65.66s
  ```

  Both blockwise geometries lose to the single pass, and the configured 300/45
  loses by a factor of two. The long file confirms it at scale: `sample-full`
  scores 30.24% / 23.87% under 300/45 against 14.36% / 6.15% single pass, and
  loses 250.6 s of one speaker to the other versus 43.9 s. Files of 300 s or
  less are unaffected either way: one block is one pass.

  **The shipped geometry is currently 300/45, and that is indefensible on these
  numbers**: it doubles confusion against the single pass. The 25 s overlap only
  loses less, because its boundaries happen to link cleanly; the overlap length
  is not a quality dial. Until the chain is replaced, blocks should stay
  opt-in and the default must remain the single pass.

  The default single-pass turn map is byte-identical to the recorded baseline.

- **The chained assignment is not good enough to ship.** It is fragile in a way
  the numbers show directly: a block where the overlap contains only one voice
  has no evidence for the other label, and one wrong link inverts every block
  after it. That is why 300/25 loses less and 300/45 loses twice as much on the
  same file — the block boundaries moved and one link broke. The plan's cluster-mean embedding
  assignment exists precisely to replace this step; it needs the speaker
  embedding extractor bound (`sherpa_dynamic.rs` currently binds the diarizer
  and segmentation configs only), so the remaining work is FFI plus the
  centroid assignment, not more tuning of the chain.

- `run_with_progress` reports 0..1 per block and `overlap_progress` clamps with
  `.min()`, so the bar would go backwards. The blockwise path composes progress
  as `base + span * fraction` instead; the single-pass path is unchanged.

Acceptance:

- Beats, on every file in the table, the current absorption figure; `sample-long`
  (10.48 s) and `sample-full` (43.94 s) must not regress.
- Does not regress the full-file pass on the 300-600 s window (13.15% / 5.55%).
- No block where both global speakers collapse into one.
- No phantom speaker on the single-speaker check once a file exists.
- Diarize wall time stays below ASR wall time for the same file.

Explicit non-goals: frame-level Viterbi smoothing, forced alignment,
overlap-aware segmentation, incremental attribution inside the ASR path.

Overlap justification: Sherpa already emits both speakers on powerset overlap
frames, reference overlap is ~1% of speech, and the transcript UI renders one
speaker per line. The ceiling does not pay for the work.

## Phase 4 - UI, cache, progress

- (done) `DIARIZER_VERSION` in the cache key, mirroring `tokens-v3`.
  `models::diarizer_id()` only changes when model files change, so Phase 3 would
  otherwise serve stale single-pass turns. Landed in `534c79f`; bump the const
  with any change to the turn map.
- (done) The key mismatch: `gui/src/backend.rs` reads with `cache::key` (file
  bytes) while the pipeline writes with `samples_hash` (decoded samples), so GUI
  cache reads always missed. `e8c1c6f` writes the turns under both keys, since a
  restored queue has the file and no samples.
- Include block geometry and embedding model in per-block cache keys.
- `Diarizing block 3/12`, with a correct base/span.
- `Only one speaker detected` when the global result is one centroid.

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

1. Finish Phase 3: replace the chained block labels with cluster-mean embedding
   assignment. This needs the speaker embedding extractor bound in
   `core/src/sherpa_dynamic.rs`, which is not there yet.
2. Re-measure both geometries (300/45 and 300/25) against the eight-file table.
   If 300/45 still loses on every file, do not ship it; the chain is the defect,
   not the overlap length.
3. (done, opt-in) Split collapsed turns by embedding drift:
   `HIPPO_TRANSCRIBE_DIARIZE_SPLIT=1` takes `hour-conversation-short-1` from
   53.89% to 27.18% and `mid-2` from 33.45% to 28.22%, at the cost of +0.7 and
   +0.2 points on `sample-long`/`sample-full`. Two identity rules were measured
   and rejected; see `tests/diarization/README.md`. Promoting it to default
   needs the two small regressions removed.
4. Cache/progress corrections (Phase 4 remainder).
5. Windows and performance validation (Phase 5).

Viterbi smoothing, overlap research, forced alignment, and incremental
attribution are deferred, each with a documented reason above.
