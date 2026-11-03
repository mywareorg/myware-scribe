#!/usr/bin/env python3
"""Score a timestamped transcript against a timestamped reference.

Each output line is credited with the reference speaker that covers most of its
time span. Speaker labels are aligned by the best one-to-one mapping, because
the two files can name speakers differently.
"""
import itertools
import re
import sys

LINE = re.compile(r"^\[(\d+):(\d+)(?::(\d+))?\]\s*(?:([^:]{1,24}):\s*)?(.*)$")


def parse(path):
    rows = []
    for raw in open(path, encoding="utf-8"):
        m = LINE.match(raw.strip())
        if not m:
            continue
        a, b, c, spk, text = m.groups()
        if c is None:
            start = int(a) * 60 + int(b)
        else:
            start = int(a) * 3600 + int(b) * 60 + int(c)
        label = spk.strip() if spk else None
        rows.append((start, label, text.strip()))
    return rows


def spans(rows, total):
    out = []
    for i, (start, spk, _) in enumerate(rows):
        end = rows[i + 1][0] if i + 1 < len(rows) else total
        out.append((start, max(end, start), spk))
    return out


def main(ref_path, hyp_path):
    ref = parse(ref_path)
    hyp = parse(hyp_path)
    if not ref or not hyp:
        print("empty input")
        return 1
    total = max(r[0] for r in ref) + 10
    ref_spans = spans(ref, total)
    hyp_spans = spans(hyp, total)

    # confusion[hyp_label][ref_label] = seconds
    confusion = {}
    unknown = 0.0
    for hstart, hend, hspk in hyp_spans:
        votes = {}
        for rstart, rend, rspk in ref_spans:
            overlap = min(hend, rend) - max(hstart, rstart)
            if overlap > 0:
                votes[rspk] = votes.get(rspk, 0.0) + overlap
        dur = hend - hstart
        if not votes or hspk is None:
            unknown += dur
            continue
        truth = max(votes, key=votes.get)
        row = confusion.setdefault(hspk, {})
        row[truth] = row.get(truth, 0.0) + dur

    hyp_labels = list(confusion)
    ref_labels = sorted({r for row in confusion.values() for r in row})
    best = None
    for perm in itertools.permutations(ref_labels, min(len(hyp_labels), len(ref_labels))):
        mapping = dict(zip(hyp_labels, perm))
        score = sum(confusion[h].get(mapping[h], 0.0) for h in hyp_labels)
        if best is None or score > best[0]:
            best = (score, mapping)

    score, mapping = best
    scored = sum(sum(row.values()) for row in confusion.values())
    print(f"reference:  {ref_path}")
    print(f"hypothesis: {hyp_path}")
    print(f"mapping:    {mapping}")
    print(f"scored {scored:7.1f}s  unscored {unknown:7.1f}s")
    if scored:
        print(f"agreement {100.0 * score / scored:.1f}%")
    for h in hyp_labels:
        secs = sum(confusion[h].values())
        ok = confusion[h].get(mapping[h], 0.0)
        print(f"  {h:>12s}: {secs:7.1f}s scored, {100.0 * ok / secs:5.1f}% correct")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], sys.argv[2]))
