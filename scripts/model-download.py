#!/usr/bin/env python3
"""Download Hugging Face models for the transiel pipeline into models/."""

import argparse
import getpass
import os
import sys
from pathlib import Path

try:
    from huggingface_hub import HfApi, hf_hub_download, snapshot_download
except ImportError:
    sys.exit("huggingface_hub is required: pip install huggingface_hub")

try:
    from tqdm import tqdm
except ImportError:
    def tqdm(iterable, **kwargs):
        return iterable

MODELS = {
    "diarization": [
        "BUT-FIT/diarizen-wavlm-large-s80-md-v2",
        "pyannote/speaker-diarization-community-1",
    ],
    "transcription": [
        "Systran/faster-whisper-base.en",
    ],
    "embedding": [
        "pyannote/wespeaker-voxceleb-resnet34-LM",
    ],
}

REPO_ROOT = Path(__file__).resolve().parent.parent
MODELS_DIR = REPO_ROOT / "models"


def local_dir(repo_id):
    return MODELS_DIR / repo_id.split("/")[-1]


def is_downloaded(repo_id):
    try:
        snapshot_download(
            repo_id=repo_id,
            local_dir=str(local_dir(repo_id)),
            local_files_only=True,
        )
        return True
    except Exception:  # noqa: BLE001
        return False


def download(repo_id, token):
    dest = local_dir(repo_id)
    dest.mkdir(parents=True, exist_ok=True)
    print(f"[+] {repo_id} -> {dest}")

    api = HfApi()
    files = api.list_repo_files(repo_id, token=token)
    for filename in tqdm(
        files,
        desc=repo_id.split("/")[-1],
        unit="file",
        leave=True,
    ):
        hf_hub_download(
            repo_id=repo_id,
            filename=filename,
            local_dir=str(dest),
            token=token,
            tqdm_class=tqdm,
        )


def main():
    parser = argparse.ArgumentParser(description="Download Hugging Face models")
    parser.add_argument(
        "repos",
        nargs="*",
        help="optional repo ids; defaults to all configured models",
    )
    parser.add_argument(
        "--token",
        default=os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN"),
        help="Hugging Face token (default: $HF_TOKEN); required for gated repos",
    )
    args = parser.parse_args()

    repos = args.repos or [r for group in MODELS.values() for r in group]

    pending = [r for r in repos if not is_downloaded(r)]
    if not pending:
        print("All models already present.")
        return

    token = args.token
    if not token:
        token = getpass.getpass("Hugging Face token: ").strip() or None

    failures = []
    for repo_id in pending:
        try:
            download(repo_id, token)
        except Exception as exc:  # noqa: BLE001
            print(f"[!] failed {repo_id}: {exc}", file=sys.stderr)
            failures.append(repo_id)

    if failures:
        sys.exit(f"Failed: {', '.join(failures)}")
    print("Done.")


if __name__ == "__main__":
    main()
