#!/usr/bin/env python3
"""Run DiariZen diarization and emit speaker turns as JSON.

Must be executed with the dedicated DiariZen interpreter (see .venv-diarizen),
because DiariZen pins an old torch and bundles its own pyannote.audio fork.
"""

import argparse
import json
import sys
from pathlib import Path

import toml


def build_config_parse(hub, num_speakers, min_speakers, max_speakers):
    config = toml.load((Path(hub) / "config.toml").as_posix())
    inference_args = config["inference"]["args"]
    clustering_args = config["clustering"]["args"]

    if num_speakers is not None:
        clustering_args["min_speakers"] = num_speakers
        clustering_args["max_speakers"] = num_speakers
    else:
        if min_speakers is not None:
            clustering_args["min_speakers"] = min_speakers
        if max_speakers is not None:
            clustering_args["max_speakers"] = max_speakers

    return {
        "inference": {"args": inference_args},
        "clustering": {"args": clustering_args},
    }


def main():
    parser = argparse.ArgumentParser(description="DiariZen diarization runner")
    parser.add_argument("--hub", required=True, help="DiariZen model directory")
    parser.add_argument("--embedding", required=True, help="speaker embedding model file")
    parser.add_argument("--audio", required=True, help="audio file to diarize")
    parser.add_argument("--output-json", required=True, help="where to write turns JSON")
    parser.add_argument("--num-speakers", type=int, default=None)
    parser.add_argument("--min-speakers", type=int, default=None)
    parser.add_argument("--max-speakers", type=int, default=None)
    args = parser.parse_args()

    hub = Path(args.hub).resolve()
    if not (hub / "config.toml").is_file():
        sys.exit(f"DiariZen hub not found or incomplete: {hub}")
    if not Path(args.embedding).is_file():
        sys.exit(f"embedding model not found: {args.embedding}")
    audio = Path(args.audio).resolve()
    if not audio.is_file():
        sys.exit(f"audio file not found: {audio}")

    from diarizen.pipelines.inference import DiariZenPipeline

    config_parse = build_config_parse(
        hub, args.num_speakers, args.min_speakers, args.max_speakers
    )

    pipeline = DiariZenPipeline(
        diarizen_hub=hub,
        embedding_model={"checkpoint": str(Path(args.embedding).resolve())},
        config_parse=config_parse,
    )

    result = pipeline(str(audio))

    turns = []
    for segment, _, label in result.itertracks(yield_label=True):
        turns.append(
            {"start": float(segment.start), "end": float(segment.end), "label": str(label)}
        )
    turns.sort(key=lambda t: (t["start"], t["end"]))

    Path(args.output_json).write_text(json.dumps({"turns": turns}), encoding="utf-8")


if __name__ == "__main__":
    main()
