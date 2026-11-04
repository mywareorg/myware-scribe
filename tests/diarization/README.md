# Diarization references

`*.rttm` here are hand-corrected references, each derived from a transcript
corrected in the app (double-click to flip a speaker, right-click to edit,
merge or split). They are not diarizer output: nothing in this directory is
generated from the system under test.

Boundaries are ASR line starts capped by a words-per-second estimate, so these
references measure **speaker attribution**, not exact turn timing. Quote DER at
both collars; the 0.25 s figure carries boundary disagreement that no diarizer
can remove.

## References

Eight corrected transcripts, each converted with `from-transcript`:

```
for f in sample-end sample-short hour-conversation-short-1 hour-conversation-short-2 \
         hour-conversation-mid-1 hour-conversation-mid-2 sample-long sample-full; do
  scripts/diarize-score.py from-transcript "audio/$f-accuracy-transcription-2speakers-ts.txt" \
      "$f" --out "tests/diarization/$f.rttm"
done
```

The 83-minute `hour-conversation-full` is deliberately left out: its content is
covered by the others and correcting 2559 lines buys no new failure mode.

Content overlap: `sample-short`, `sample-long` and `sample-end` are excerpts of
`sample-full` (text alignment confirms `sample-long` at a +1 line offset, labels
agreeing 25/27). They are scored against their own file and are **region
checks**, not independent evidence. Do not average across files.

## Baseline (2026-11-03)

Debug build, balanced preset, `--num-speakers 2`, after `clean_turns`:

```
file                         ref s   DER@0.25   DER@1.00   conf s  absorb s    short  worst blk
hour-conversation-mid-1      201.2     34.97%     26.76%    60.99     39.15    27/52     34.97%
hour-conversation-mid-2      193.6     33.45%     25.47%    53.77     43.61    25/50     33.09%
hour-conversation-short-1     75.8     53.89%     48.13%    36.53     35.03    11/20     53.89%
hour-conversation-short-2     79.8     13.57%      1.82%     7.23      4.67    10/20     13.57%
sample-end                    82.4     10.60%      3.58%     3.61      2.38    14/20     10.60%
sample-full                 1471.4     14.36%      6.15%    73.10     43.94   77/132     18.54%
sample-long                  429.3     10.62%      2.76%    18.13     10.48    40/59     13.13%
sample-short                  88.9     10.36%      1.60%     2.11      1.26      1/6     10.36%
```

Reproduce with:

```
scripts/diarize-score.py record --bin target/debug/hippo-transcribe --speakers 2 \
    --out audio/eval/baseline <audio>...
scripts/diarize-score.py report --ref-dir tests/diarization --hyp-dir audio/eval/baseline
```

`report` prints one row per file: DER at each collar, confusion seconds, the
largest one-directional absorption, short-turn recall, and the worst block
(slivers under 5 s of reference speech are ignored).

### What the table says

- **The spread is the finding.** 10% to 54% DER at the same settings. Any claim
  based on `sample-long` alone understated the problem by a factor of five.
- **`hour-conversation-short-1` is the real failure.** 48.13% even at a 1.0 s
  collar, i.e. attribution, not boundaries. The hypothesis collapses 10.4-80.0 s
  into one speaker while the reference alternates roughly every 2 s. This is
  rapid turn-taking, and the diarizer does not track it at all.
- **`hour-conversation-mid-1`/`-2` fail the same way**, less severely: one
  speaker is absorbed at 39-44 s.
- **`hour-conversation-short-2` is boundary noise, not speaker error**: 13.57%
  at 0.25 s collapses to 1.82% at 1.0 s.
- **Absorption is everywhere.** Every file loses one speaker's speech to the
  other; it is the single recurring mechanism.

## Blockwise vs single pass

The blockwise path (`HIPPO_TRANSCRIBE_DIARIZE_BLOCKS=1`) only changes files
longer than one 300 s block. On those it is worse, badly:

```
file            ref s   single @0.25  single @1.0   blocks @0.25  blocks @1.0
sample-long     429.3          10.62%        2.76%         21.45%       15.71%
sample-full    1471.4          14.36%        6.15%         30.24%       23.87%
```

`sample-full` also loses 250.6 s of one speaker to the other under blocks versus
43.9 s in the single pass. Files at or under 300 s are byte-identical either
way, which is why the other rows are unchanged.

Verdict: **do not enable blocks.** The chained label assignment is the defect —
one wrong link in an overlap propagates through every later block. It has to be
replaced by cluster-mean embedding assignment before this path ships.

## Turn splitting (opt-in)

`HIPPO_TRANSCRIBE_DIARIZE_SPLIT=1` splits turns of 15 s or more when their
1.5 s windows form two clusters that differ by cosine <= 0.80. Each half is
assigned by comparing it against per-label centroids built from the nearest
turns of that label, excluding the turn under test.

```
file                        single @0.25/@1.0     split @0.25/@1.0
hour-conversation-short-1      53.89 / 48.13       27.18 / 17.17
hour-conversation-mid-2        33.45 / 25.47       28.22 / 16.50
hour-conversation-mid-1        34.97 / 26.76       34.97 / 26.76
hour-conversation-short-2      13.57 /  1.82       13.57 /  1.82
sample-end                     10.60 /  3.58       10.60 /  3.58
sample-short                   10.36 /  1.60       10.36 /  1.60
sample-long                    10.62 /  2.76       11.31 /  3.92
sample-full                    14.36 /  6.15       14.55 /  6.27
```

Two identity rules were measured and rejected, so they are not retried:

- **Continuity** (the half that abuts speaker X is X): forces the closing half
  onto the following speaker, so every long turn bracketed by the other voice
  is cut. `sample-short` 10.36% -> 35.02%, `mid-1` 34.97% -> 49.31%,
  `sample-full` 14.36% -> 33.48%.
- **Continuity verified against neighbour audio**: 1.5 s neighbour windows are
  too noisy to confirm which half continues which neighbour. `short-1` fell
  back to 48.65% and `sample-long` rose to 13.62%.

- **Giving the weaker half the other label when both halves prefer the same
  one**: the clusters are real, so the split was kept and the identity forced.
  Wrong far more often than right: `mid-1` 34.97% -> 50.83%, `sample-long`
  10.62% -> 49.19%. Reject the split instead.

The remaining regression is small and confined to the two files where the
diarizer is already close to the reference; splitting is worth enabling on
files like `short-1` where one speaker is collapsed for tens of seconds.

## Embedding model is not a per-family lever

`HIPPO_TRANSCRIBE_EMBEDDING_MODEL` swaps the speaker model with no code change.
Comparing the default CAM++ against resnet34 across all eight files, same
pipeline, same `--num-speakers 2`:

```
file          CAM++ @0.25/@1.0     resnet34 @0.25/@1.0
short-1          53.89 / 48.13       17.36 / 11.03
mid-1            34.97 / 26.76       35.25 / 27.49
mid-2            33.45 / 25.47       49.16 / 37.91
short-2          13.57 /  1.82       13.73 /  3.61
end              10.60 /  3.58       37.38 / 31.04
full             14.36 /  6.15       14.71 /  6.96
long             10.62 /  2.76       34.45 / 26.33
short            10.36 /  1.60       10.36 /  1.60
```

The hypothesis was that the two recording families want different models. They
do not: resnet34 wins on **one** file (`short-1`, the collapsed one) and is
worse or equal on the other seven, including `mid-2` and `end` from both
families. `sample-short` produces byte-identical turns under both, so the model
barely matters there.

CAM++ stays the default. `short-1` is an outlier where a different embedding
happens to suit the audio, and nothing in the file identifies it as such.

## Dense window track (measured, removed)

A 1.5 s window track (0.75 s hop, Viterbi with a switch penalty 0.02) that
replaces the diarizer's turn geometry, with centroids taken either from the
diarizer's own turns or from clustering the windows themselves. The idea was to
fix under-segmentation, which no post-pass over turns can.

```
file          baseline @0.25/@1.0   diarizer seed      k-means seed
short-1          53.89 / 48.13      45.64 / 42.26      13.24 /  5.59
mid-1            34.97 / 26.76      34.30 / 27.44      38.50 / 33.42
mid-2            33.45 / 25.47      25.30 / 20.72      28.19 / 19.83
short-2          13.57 /  1.82      16.66 /  9.78      43.24 / 40.86
end              10.60 /  3.58      11.44 /  1.32      19.54 / 15.12
full             14.36 /  6.15      14.81 /  6.64      21.09 / 13.07
long             10.62 /  2.76      11.67 /  4.73      20.35 / 15.91
short            10.36 /  1.60      19.01 /  8.79      44.39 / 41.89
```

The diarizer seed is a wash: it wins `mid-2` and half of `short-1`, and loses
the files that were already at 10-13%. The k-means seed is spectacular on
`short-1` (better than the 21% oracle-span ceiling) and ruins six files,
including two at 10-13%.

Seeding the k-means with restarts and picking the lowest-inertia assignment
made it worse, not better: `short-1` 13.24% -> 14.39%, `short-2` 43.24% ->
54.60%, `sample-short` 44.39% -> 48.94%, `sample-long` unchanged at 20.35%.
Inertia rewards a balanced split by whatever feature varies (level, music),
not by speaker identity.

Deleted rather than kept behind a flag: it costs roughly a doubling of the
diarize stage and is neutral at best.

## Ceiling test: reference spans through our own clustering

A diagnostic, not a result: take the reference's own spans (perfect
segmentation), embed each with the speaker model, and cluster with two-means.
That isolates clustering quality from segmentation quality.

```
file        intra-speaker cosine   inter-speaker   oracle-span DER@0.25/@1.0
short-1          0.431 (1186)        0.372 (1160)         21.28% / 19.69%
sample-long      0.467 (15196)       0.388 (13965)        (not scored)
```

Two conclusions:

- **The embedding margin is small on both files** (0.06-0.08 cosine), so the
  model is not the thing separating these speakers cleanly. It is still enough
  to cluster with, because sample-long scores 10.62% with the same margin.
- **Most of short-1's error is under-segmentation, not label error.** With the
  reference's own spans it scores 21.28%, against 53.89% today and 27.18% with
  drift-splitting. The file is 120 s long with 69 reference turns while the
  diarizer emits 21 raw units - roughly half the granularity the audio needs.

So the next change should be a dense window track (1.5 s windows, 0.75 s hop,
Viterbi with a switch penalty, centroids bootstrapped from the diarizer's
turns), not another post-pass over `Vec<Turn>`. Its ceiling on short-1 is
~21%; it must not move sample-long.

Reproduce with the ignored diagnostic:

```
DIARIZE_AUDIO=audio/hour-conversation-short-1.mp3 \
DIARIZE_RTTM=tests/diarization/hour-conversation-short-1.rttm \
DIARIZE_OUT=/tmp/oracle.tsv \
cargo test -p hippo-transcribe-core --lib measure_reference_separability \
    -- --ignored --nocapture
scripts/diarize-score.py score --ref tests/diarization/hour-conversation-short-1.rttm \
    --hyp /tmp/oracle.tsv
```

## Re-clustering the turns (measured, not shipped)

An experiment, recorded so it is not repeated without new evidence: embed every
turn with the speaker model, run 2-means over those vectors, and replace the
diarizer's labels.

```
file            diarizer labels   re-clustered     coherence gate
mid-1            34.97%            29.95%           rejected (34.97%)
short-1          53.89%            54.07%           rejected
sample-long      10.62%            22.76%           rejected
```

It helps exactly the file whose global clustering is wrong and destroys one
where it is right. The intended discriminator — mean within-label cosine for
each labelling — chose the diarizer's labels on all three files, including
`mid-1`, so it cannot tell the good case from the bad.

What the experiment does show: `mid-1`'s 35% is a clustering error, not a
segmentation error. Any future fix has to replace the clustering on files like
it, with a signal better than within-label coherence.

## Independent blocks are not an improvement

An ablation, not a test of the shipped blockwise path: the same 300-600 s window
of `sample-long`, diarized once as part of the full file and once as a 5-minute
clip on its own.

```
sample-long, 300-600 s window      DER@0.25   DER@1.0   absorption
full-file pass, scored on window     13.15%      5.55%        5.6s
5-min clip diarized alone            15.89%      8.26%       13.7s
```

Splitting into independent blocks and trusting their local labels is worse:
fewer embeddings per cluster, no cross-block evidence. That is why the blockwise
path chains labels through the overlap, and why the remaining work is cluster-mean
embedding assignment rather than more blocks.

The earlier version of this note claimed local diarization was better (12.56% vs
16.10%). That was measured against the previous, less-edited reference; the
finer reference reversed the result. Re-run any headline number after the
references change.

## Monologue check

`sample-full` has a single-speaker run at 9:31-12:22 (2.9 min). Score the
hypothesis restricted to that window and assert one label; no new recording:

```
scripts/diarize-score.py score --ref tests/diarization/sample-full.rttm \
    --hyp audio/eval/baseline/sample-full.tsv --ref-range 571 742
scripts/diarize-score.py speakers audio/eval/baseline/sample-full.tsv \
    --range 571 742 --dominant 0.95
```

`--dominant` is the right check, not `--expect 1`: a phantom label usually
covers a few seconds of a one-voice stretch. Current single-pass output gives
97.1% `spk1` / 2.9% `spk0` in that window — passing, but with a phantom label
present.
