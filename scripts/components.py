#!/usr/bin/env python3
"""Component registry for transiel: swappable diarization / embedding / transcription.

This module is the single source of truth for what models are available and how
the speed / balanced / accuracy presets map onto them. It is imported by both
``transiel.py`` and ``model-download.py``.

Run ``python scripts/components.py --list`` to see the available components.
"""

import argparse
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MODELS_DIR = REPO_ROOT / "models"

NUM_SPEAKERS = 2

# Each descriptor:
#   backend/loader : how to build/load it
#   model          : Hugging Face repo id
#   local          : directory name under models/
#   required_files : files used for the completeness check
#   gated          : requires a Hugging Face token
DIARIZERS = {
    "diarizen-large": {
        "backend": "diarizen",
        "model": "BUT-FIT/diarizen-wavlm-large-s80-md-v2",
        "local": "diarizen-wavlm-large-s80-md-v2",
        "required_files": [
            "config.toml",
            "config.json",
            "pytorch_model.bin",
            "plda/plda.npz",
            "plda/xvec_transform.npz",
        ],
        "gated": False,
    },
    "diarizen-base": {
        "backend": "diarizen",
        "model": "BUT-FIT/diarizen-wavlm-base-s80-md",
        "local": "diarizen-wavlm-base-s80-md",
        "required_files": [
            "config.toml",
            "config.json",
            "pytorch_model.bin",
            "plda/plda.npz",
            "plda/xvec_transform.npz",
        ],
        "gated": False,
    },
    "pyannote-seg3": {
        "backend": "pyannote",
        "model": "pyannote/segmentation-3.0",
        "local": "segmentation-3.0",
        "required_files": ["config.yaml", "pytorch_model.bin"],
        "gated": True,
    },
}

EMBEDDINGS = {
    "wespeaker-resnet34": {
        "loader": "pyannote",
        "model": "pyannote/wespeaker-voxceleb-resnet34-LM",
        "local": "wespeaker-voxceleb-resnet34-LM",
        "required_files": ["config.yaml", "pytorch_model.bin"],
        "gated": False,
    },
}

TRANSCRIBERS = {
    "whisper-turbo": {
        "model": "deepdml/faster-whisper-large-v3-turbo-ct2",
        "local": "faster-whisper-large-v3-turbo-ct2",
        "required_files": ["model.bin", "config.json", "tokenizer.json"],
        "gated": False,
    },
    "whisper-base-en": {
        "model": "Systran/faster-whisper-base.en",
        "local": "faster-whisper-base.en",
        "required_files": ["model.bin", "config.json", "tokenizer.json"],
        "gated": False,
    },
}

# name -> components + tuning. Anything other than the three component keys is
# passed through as tuning (segmentation_step, beam_size, threads, ...).
PRESETS = {
    "speed": {
        "diarizer": "pyannote-seg3",
        "embedding": "wespeaker-resnet34",
        "transcriber": "whisper-base-en",
        "segmentation_step": 0.3,
        "beam_size": 1,
        "threads": 0,  # 0 => use all cores
    },
    "balanced": {
        "diarizer": "pyannote-seg3",
        "embedding": "wespeaker-resnet34",
        "transcriber": "whisper-turbo",
        "segmentation_step": 0.2,
    },
    "accuracy": {
        "diarizer": "diarizen-large",
        "embedding": "wespeaker-resnet34",
        "transcriber": "whisper-turbo",
        "segmentation_step": 0.1,
    },
}

DEFAULT_PRESET = "accuracy"

_CATEGORIES = {
    "diarizer": DIARIZERS,
    "embedding": EMBEDDINGS,
    "transcriber": TRANSCRIBERS,
}


def model_path(descriptor):
    """Local directory for a component descriptor."""
    return MODELS_DIR / descriptor["local"]


def resolve(preset=DEFAULT_PRESET, diarizer=None, embedding=None, transcriber=None):
    """Resolve a preset plus per-component overrides into concrete descriptors."""
    if preset not in PRESETS:
        raise KeyError(f"unknown preset '{preset}' (have: {', '.join(PRESETS)})")

    base = PRESETS[preset]
    names = {
        "diarizer": diarizer or base["diarizer"],
        "embedding": embedding or base["embedding"],
        "transcriber": transcriber or base["transcriber"],
    }

    resolved = {"preset": preset}
    for category, name in names.items():
        registry = _CATEGORIES[category]
        if name not in registry:
            raise KeyError(
                f"unknown {category} '{name}' (have: {', '.join(registry)})"
            )
        resolved[f"{category}_name"] = name
        resolved[category] = registry[name]

    resolved["tuning"] = {
        key: value
        for key, value in base.items()
        if key not in ("diarizer", "embedding", "transcriber")
    }
    return resolved


def assets():
    """All downloadable assets: (repo_id, local_dir_name, required_files, gated)."""
    seen = set()
    result = []
    for registry in _CATEGORIES.values():
        for descriptor in registry.values():
            repo_id = descriptor["model"]
            if repo_id in seen:
                continue
            seen.add(repo_id)
            result.append(
                (
                    repo_id,
                    descriptor["local"],
                    list(descriptor.get("required_files", [])),
                    bool(descriptor.get("gated", False)),
                )
            )
    return result


def _print_list():
    for category, registry in _CATEGORIES.items():
        print(f"{category}s:")
        for name, descriptor in registry.items():
            backend = descriptor.get("backend") or descriptor.get("loader") or ""
            gated = "gated" if descriptor.get("gated") else "     "
            print(
                f"  {name:24s} [{backend:10s}] {gated}  {descriptor['model']}"
            )
    print("presets:")
    for name, preset in PRESETS.items():
        print(
            f"  {name:10s} diarizer={preset['diarizer']} "
            f"embedding={preset['embedding']} transcriber={preset['transcriber']}"
        )
    print(f"\ndefault preset: {DEFAULT_PRESET}")


def main():
    parser = argparse.ArgumentParser(description="transiel component registry")
    parser.add_argument("--list", action="store_true", help="list components and presets")
    args = parser.parse_args()
    if args.list:
        _print_list()
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
