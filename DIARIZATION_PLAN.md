# Diarization Improvement Plan

Status: Phase 0 complete; Phase 1 dropped; Phase 4 cache work landed; Phase 3 next.

Goal: improve speaker isolation on long recordings without cloud services,
without gated models, and without changing the local-only architecture.

## What the baseline actually says

Measured on `audio/sample-long.mp3` (10 min, two speakers) against
`tests/diarization/sample-long.rttm`:

```text
DER @0.25s 13.04%   DER @1.0s 7.08%
missed 1.08%  false alarm 4.66%  confusion 7.29%
confusion by ref turn length: <1s 9.4%, 1-3s 27.2%, >3s 63.4%
diarize stage 31.34s for 600s audio (0.052x realtime, 8 threads)
```

Three conclusions, each of which changed the plan:

1. **The error is one-directional.** SPEAKER 2's speech is credited to
   SPEAKER 1 for ~24 s across the file. That is absorption/merging, not random
   frame noise.
2. **It is not drift.** The two 5-minute blocks lose about the same amount
   (10.7 s vs 13.5 s); the second block's higher DER comes from false alarm.
3. **It is not a short-turn problem.** Only 9.4% of confusion sits inside turns
   under 1 s; 63.4% sits inside turns over 3 s. Half the headline figure is
   collar noise from estimated reference boundaries.

A 5-minute clip cut from 300-600 s and diarized *alone* scores 12.56% / 4.86%
(DER@0.25 / DER@1.0) and halves the absorption to 5.7 s on the same reference
window. Local diarization is materially better on the same audio, so replacing
the global clustering step has a measured target.

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

Open item: no single-speaker file in the repo, so the phantom-speaker assertion
is implemented but unexercised.

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

## Phase 2 - Decidability gate (answered)

The gate was run as part of Phase 0 and passed: local diarization on a 5-minute
slice beats the full-file pass on the same slice (see above). Phase 3 therefore
proceeds. Keep the script in `tests/diarization/README.md` as the regression
check for any future blockwise change.

## Phase 3 - Blockwise diarization, cluster-level assignment

One phase, replacing the earlier blockwise / centroid / Viterbi trio.

Blocks:

- 5 minutes with 20-30 s overlap; benchmark 2 minutes with 15 s separately.
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
- Measured on `audio/sample-long.mp3` against `tests/diarization/sample-long.rttm`,
  `--num-speakers 2`, debug build:

  ```text
  single pass            13.04% @0.25s   7.08% @1.0s   confusion 31.73s
  blocks 300 / 25        11.45% @0.25s   4.63% @1.0s   confusion 25.07s
  blocks 240 / 60        13.55% @0.25s   6.80% @1.0s   confusion 34.20s
  blocks 180 / 60        29.01% @0.25s  20.77% @1.0s   confusion 101.62s
  blocks 300 / 45        23.43% @0.25s  19.15% @1.0s   confusion  77.36s
  ```

  The default single-pass turn map is byte-identical to the recorded baseline.

- **The chained assignment is not good enough to ship.** It is fragile in a way
  the numbers show directly: a block where the overlap contains only one voice
  has no evidence for the other label, and one wrong link inverts every block
  after it. That is why 300/25 wins and 300/45 loses on the same file — the
  block boundaries moved and one link broke. The plan's cluster-mean embedding
  assignment exists precisely to replace this step; it needs the speaker
  embedding extractor bound (`sherpa_dynamic.rs` currently binds the diarizer
  and segmentation configs only), so the remaining work is FFI plus the
  centroid assignment, not more tuning of the chain.

- `run_with_progress` reports 0..1 per block and `overlap_progress` clamps with
  `.min()`, so the bar would go backwards. The blockwise path composes progress
  as `base + span * fraction` instead; the single-pass path is unchanged.

Acceptance:

- Beats the Phase 2 slice numbers (12.56% / 4.86%, absorption 5.7 s on
  300-600 s) without regressing the full file.
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

1. Blockwise with cluster-level assignment (Phase 3).
2. Cache/progress corrections (Phase 4 remainder).
3. Windows and performance validation (Phase 5).

Viterbi smoothing, overlap research, forced alignment, and incremental
attribution are deferred, each with a documented reason above.
