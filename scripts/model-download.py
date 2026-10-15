#!/usr/bin/env python3
"""Download Hugging Face models for the transiel pipeline into models/."""

import argparse
import getpass
import os
import sys
from pathlib import Path

try:
    from huggingface_hub import snapshot_download
    from huggingface_hub.utils import tqdm as hf_tqdm
except ImportError:
    sys.exit("huggingface_hub is required: pip install huggingface_hub")

import components

REPO_ROOT = Path(__file__).resolve().parent.parent
MODELS_DIR = components.MODELS_DIR

# repo_id -> (local dir name, required files, gated)
ASSETS = {
    repo_id: {"local": local, "required_files": required, "gated": gated}
    for repo_id, local, required, gated in components.assets()
}
ALL_REPOS = list(ASSETS)


def local_dir(repo_id):
    info = ASSETS.get(repo_id)
    if info is not None:
        return MODELS_DIR / info["local"]
    return MODELS_DIR / repo_id.split("/")[-1]


def verify(repo_id):
    """Return a list of missing required files for a repo (empty if complete)."""
    dest = local_dir(repo_id)
    info = ASSETS.get(repo_id)
    if info is None:
        return [] if dest.is_dir() else ["<directory>"]
    missing = [name for name in info["required_files"] if not (dest / name).is_file()]
    return missing


def is_gated(repo_id):
    info = ASSETS.get(repo_id)
    return bool(info and info["gated"])


def download(repo_id, token):
    """Download a repo into models/<name>/ showing aggregated byte progress bars.

    ``snapshot_download`` already aggregates per-file byte progress into shared
    transfer/reconstruction bars (HTTP and xet); we only relabel them with the
    repo name so it is clear which model is downloading.
    """
    dest = local_dir(repo_id)
    dest.mkdir(parents=True, exist_ok=True)
    print(f"[+] {repo_id} -> {dest}")

    name = repo_id.split("/")[-1]

    class RepoTqdm(hf_tqdm):
        def __init__(self, *args, **kwargs):
            desc = kwargs.get("desc")
            if desc:
                kwargs["desc"] = f"{name}: {desc}"
            super().__init__(*args, **kwargs)

    snapshot_download(
        repo_id=repo_id,
        local_dir=str(dest),
        token=token,
        tqdm_class=RepoTqdm,
    )


def repos_for_preset(preset):
    resolved = components.resolve(preset)
    return [
        resolved["diarizer"]["model"],
        resolved["embedding"]["model"],
        resolved["transcriber"]["model"],
    ]


def repos_for_component(spec):
    if ":" not in spec:
        sys.exit(f"--component expects category:name (e.g. diarizer:pyannote-seg3), got '{spec}'")
    category, name = spec.split(":", 1)
    registry = {
        "diarizer": components.DIARIZERS,
        "embedding": components.EMBEDDINGS,
        "transcriber": components.TRANSCRIBERS,
    }.get(category)
    if registry is None:
        sys.exit(f"unknown component category '{category}'")
    if name not in registry:
        sys.exit(f"unknown {category} '{name}' (have: {', '.join(registry)})")
    return [registry[name]["model"]]


def select_repos(args):
    repos = list(args.repos)
    for preset in args.preset:
        repos += repos_for_preset(preset)
    for spec in args.component:
        repos += repos_for_component(spec)
    if not repos:
        repos = list(ALL_REPOS)
    seen = set()
    ordered = []
    for repo in repos:
        if repo not in seen:
            seen.add(repo)
            ordered.append(repo)
    return ordered


def main():
    parser = argparse.ArgumentParser(description="Download Hugging Face models")
    parser.add_argument(
        "repos",
        nargs="*",
        help="optional repo ids; defaults to all configured models",
    )
    parser.add_argument(
        "--preset",
        action="append",
        choices=list(components.PRESETS),
        default=[],
        help="download the models for a preset (repeatable)",
    )
    parser.add_argument(
        "--speed",
        dest="preset",
        action="append_const",
        const="speed",
        help="shortcut for --preset speed",
    )
    parser.add_argument(
        "--balanced",
        dest="preset",
        action="append_const",
        const="balanced",
        help="shortcut for --preset balanced",
    )
    parser.add_argument(
        "--accuracy",
        dest="preset",
        action="append_const",
        const="accuracy",
        help="shortcut for --preset accuracy",
    )
    parser.add_argument(
        "--component",
        action="append",
        default=[],
        metavar="CATEGORY:NAME",
        help="download a single component's model (repeatable)",
    )
    parser.add_argument(
        "--token",
        default=os.environ.get("HF_TOKEN") or os.environ.get("HUGGING_FACE_HUB_TOKEN"),
        help="Hugging Face token (default: $HF_TOKEN); required for gated repos",
    )
    parser.add_argument(
        "--verify",
        action="store_true",
        help="only check that models are complete; do not download",
    )
    parser.add_argument(
        "--refresh",
        action="store_true",
        help="re-download all selected models regardless of completeness",
    )
    args = parser.parse_args()

    repos = select_repos(args)

    incomplete = {r: verify(r) for r in repos}
    incomplete = {r: missing for r, missing in incomplete.items() if missing}

    if args.verify:
        for repo_id in repos:
            missing = verify(repo_id)
            if missing:
                print(f"[!] {repo_id}: incomplete (missing {', '.join(missing)})")
            else:
                print(f"[ok] {repo_id}")
        if incomplete:
            sys.exit(1)
        return

    if args.refresh:
        pending = list(repos)
    else:
        if not incomplete:
            print("All models already present.")
            return
        for repo_id, missing in incomplete.items():
            print(f"[!] {repo_id}: incomplete (missing {', '.join(missing)})")
        pending = list(incomplete)

    token = args.token
    gated_pending = [r for r in pending if is_gated(r)]
    if gated_pending and not token:
        print(
            "[!] gated repos require a token: "
            + ", ".join(gated_pending)
            + "\n    set HF_TOKEN or pass --token, and accept the model conditions on HF"
        )
    if not token and sys.stdin.isatty():
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

    still_incomplete = {r: verify(r) for r in pending}
    still_incomplete = {r: m for r, m in still_incomplete.items() if m}
    if still_incomplete:
        sys.exit(f"Incomplete after download: {', '.join(still_incomplete)}")

    print("Done.")


if __name__ == "__main__":
    main()
