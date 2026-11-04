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
