# Diarization Improvement Plan

Status: proposed

Goal: improve speaker isolation on long recordings without cloud services,
without gated models, and without changing the local-only architecture.

## Target pipeline

```text
decode
  -> split into overlapping blocks
  -> local diarization per block
  -> segment embeddings
  -> global speaker centroids
  -> reassign segments to global speakers
  -> temporal smoothing / resegmentation
  -> speaker turns
  -> Whisper windows
  -> token-level speaker attribution
```

## Current baseline

Already present:

- pyannote segmentation 3.0 ONNX
- CAM++ embedding for the Accuracy preset
- Whisper token timestamps
- token-level speaker splitting
- one global diarization call for the whole file

Missing:

- blockwise diarization
- segment embeddings outside the sherpa clustering step
- global centroid reassignment
- temporal smoothing / resegmentation
- overlap-aware segmentation

## Model constraints

Use only ungated, locally runnable models:

- Segmentation: `sherpa-onnx-pyannote-segmentation-3-0/model.onnx`
  via the ungated ONNX mirror.
- Accuracy embedding: `wespeaker_en_voxceleb_CAM++_LM.onnx`
  from the sherpa-onnx release.
- Fallback embedding: `wespeaker_en_voxceleb_resnet34_LM.onnx`.
- ASR: existing Whisper GGML models.

Do not use the gated `pyannote/speaker-diarization-3.1` pipeline or cloud
diarization APIs.

All model files stay in `models.lock.json` with size, SHA-256, license, and
mirrors.

## Phase 0 - Evaluation harness

Before changing diarization, make quality measurable.

Deliverables:

- A small labeled set of local audio with RTTM or JSON speaker turns.
- Scripts to compute:
  - diarization error rate
  - missed speech / false alarm / speaker confusion
  - short-turn recall for turns under 1 second
  - speaker-attributed word error rate
- Per-block DER so drift is visible across a long file.

Acceptance:

- Baseline numbers for the current single-pass diarization.
- Reproducible before/after comparisons.

This prevents another unverified "sounds better" change.

## Phase 1 - Blockwise local diarization

Deliverables:

- Split decoded audio into configurable blocks.
- Initial setting: 5 minutes with 20-30 seconds of overlap.
- Benchmark 2 minutes with 15 seconds separately.
- Each block runs local diarization and returns turns.
- Per block, allow 1-2 local clusters, never force 2.
- Keep the user-selected global speaker count as the global target.

Important rule:

```text
global target: 2 speakers
per-block clusters: 1..2
```

A 2-minute monologue must not be split into two speakers just because the
global target is 2.

Acceptance:

- One-speaker block produces one local cluster.
- No phantom speaker during monologue.
- Diarization stage overhead stays within roughly 1.10-1.15x.
- End-to-end overhead stays within roughly 1.02-1.05x.

## Phase 2 - Segment embeddings and global centroids

Deliverables:

- Extract an embedding for each local turn or sub-window.
- Prefer multiple overlapping 1.5-3 second windows per turn, then average.
- L2-normalize every embedding.
- Build global centroids from confident local clusters:
  - duration-weighted mean
  - cap any single segment's influence
  - re-normalize after averaging
- Reassign every segment to the nearest global centroid.
- Assign only when:
  - best similarity clears an absolute floor
  - best speaker beats the second speaker by a margin
- Mark low-confidence segments as unknown rather than forcing a speaker.

Implementation notes:

- Add speaker-embedding-extractor symbols to `core/src/sherpa_dynamic.rs`
  and use the existing `sherpa-rs-sys` path on Linux.
- Keep the existing sherpa diarizer for local segmentation and clustering.
- Use a separate embedding pass for centroid construction and reassignment.
- Store `SegmentEmbedding { start, end, vector, duration, block, local_cluster }`.
- Store `SpeakerCentroid { vector, weight, anchor, last_update }`.

Acceptance:

- Segments from a single-speaker block all map to one global centroid.
- A speaker absent from a block gets no turns in that block.
- Centroid reassignment improves DER over the single-pass baseline.

## Phase 3 - Temporal smoothing and resegmentation

Deliverables:

- Score short frames or segments against both global centroids.
- Run Viterbi/HMM smoothing with a speaker-change penalty.
- Use smoothing to correct isolated flips and short uncertain segments.
- Recompute centroids once from accepted assignments.
- Do not update centroids from:
  - segments under roughly 0.5 seconds
  - low-margin segments
  - overlap regions

Acceptance:

- Fewer one-word speaker flips.
- Short interjections either inherit the correct surrounding speaker or remain
  unknown.
- No regression on clean two-speaker audio.

## Phase 4 - Overlap-aware segmentation

This is the hardest part. Treat it as a separate research phase.

Options to evaluate:

- pyannote segmentation 3.0 powerset output, if the ONNX model exposes
  multi-label frames.
- A second ungated segmentation model that outputs overlapping speaker
  activity.
- Keep the current single-label segmentation and mark low-confidence overlap
  regions rather than pretending they are clean turns.

Do not introduce gated models or cloud services.

Acceptance:

- Overlap regions are detected or explicitly marked uncertain.
- Mixed-speaker segments are not silently assigned to one speaker.
- If no viable ungated overlap model exists, document the limitation and keep
  overlap handling as a later improvement.

## Phase 5 - ASR attribution

Already partly implemented.

Remaining work:

- Keep token-level timestamps for Balanced and Accuracy.
- Attribute tokens to smoothed speaker turns.
- Preserve the single-word fallback rule.
- Optionally add a lightweight forced-alignment pass if Whisper token
  boundaries prove too coarse.

Acceptance:

- Word-level speaker splits align with smoothed turns.
- No mid-sentence speaker flips caused by a single token.

## Phase 6 - UI, cache, and progress

Deliverables:

- Show block progress during diarization: `Diarizing block 3/12`.
- Cache block results by audio hash, block range, model set, preset, and
  speaker count.
- Invalidate caches when the embedding model or manifest version changes.
- Keep the UI at a fixed two-speaker target; no auto-speaker selector.
- If only one speaker is found globally, show `Only one speaker detected`
  instead of inventing a second.

Acceptance:

- Re-running a file reuses block caches.
- Changing preset or embedding model does not reuse incompatible turns.
- Windows MSIX works offline after models are installed.

## Phase 7 - Windows and performance validation

Test matrix:

- CPU-only Windows build.
- GPU build with CPU fallback.
- 2-minute, 30-minute, and 2-hour files.
- Paths with spaces.
- Offline install with bundled/downloaded models.
- Clean MSIX install.

Measure:

- end-to-end runtime factor
- diarization-stage factor
- peak memory
- DER per block
- speaker-attributed WER

## Recommended order

1. Evaluation harness.
2. Blockwise local diarization with 1-2 clusters per block.
3. Segment embeddings and global centroid reassignment.
4. Viterbi smoothing.
5. Overlap-aware segmentation.
6. Cache/progress/UI.
7. Windows packaging and long-file validation.

The first three phases deliver most of the quality gain. Overlap handling is
valuable but should not block the blockwise/centroid work.
