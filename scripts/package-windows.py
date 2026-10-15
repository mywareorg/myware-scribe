#!/usr/bin/env python3
"""Build a self-contained Windows release of transiel (no Python on the target).

Run this on a Windows build machine. It assembles::

    dist/transiel/
        deploy.bat
        transiel-gui.exe
        VERSION
        scripts/            (transiel.py, components.py, ...)
        runtime/            (embeddable Python 3.11 + all dependencies)
        models/             (whisper + diarization, with a .complete marker)

and zips it to ``dist/transiel-windows-x64.zip``.

Requirements on the build machine:
  * ``gui/target/release/transiel-gui.exe`` already built (``cargo build --release``)
  * ``models/`` populated (``python scripts/model-download.py``) in this checkout
  * network access for the embeddable runtime + get-pip (unless --python-zip given)

The target machine needs none of the above: extract the zip and run ``deploy.bat``.
"""

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import urllib.request
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CLONE = REPO_ROOT / "third_party" / "DiariZen"
MODELS_DIR = REPO_ROOT / "models"
EXE = REPO_ROOT / "gui" / "target" / "release" / "transiel-gui.exe"

DIST = REPO_ROOT / "dist"
BUNDLE = DIST / "transiel"
ZIP_PATH = DIST / "transiel-windows-x64.zip"
BUILD_LOG_DIR = DIST / "build-log"

EMBEDDABLE_URL = "https://www.python.org/ftp/python/3.11.9/python-3.11.9-embed-amd64.zip"
GET_PIP_URL = "https://bootstrap.pypa.io/get-pip.py"
TORCH_CPU_INDEX = "https://download.pytorch.org/whl/cpu"

SCRIPTS_TO_SHIP = ("transiel.py", "components.py", "model-download.py")

# Files pip needs on the runtime's import path (embeddable Python is stripped).
PYTHON_DLL_GLOBS = ("python3*.zip",)
PTH_GLOB = "python3*._pth"


def log(message):
    print(f"[package] {message}", flush=True)


def fail(message):
    sys.exit(f"[package] ERROR: {message}")


def run(cmd, **kwargs):
    cmd = [str(part) for part in cmd]
    log("+ " + " ".join(cmd))
    subprocess.run(cmd, check=True, **kwargs)


def download(url, dest):
    if dest.is_file():
        log(f"Using cached {dest.name}")
        return dest
    log(f"Downloading {url}")
    dest.parent.mkdir(parents=True, exist_ok=True)
    with urllib.request.urlopen(url) as response, open(dest, "wb") as handle:
        shutil.copyfileobj(response, handle)
    return dest


def project_version():
    """Read the GUI's package version from Cargo.toml (single source of truth)."""
    cargo = (REPO_ROOT / "gui" / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^version\s*=\s*"([^"]+)"', cargo, re.MULTILINE)
    if not match:
        fail("could not read version from gui/Cargo.toml")
    return match.group(1)


def verify_models():
    """Fail loudly unless models/ contains every configured model."""
    sys.path.insert(0, str(REPO_ROOT / "scripts"))
    import components  # noqa: PLC0415

    incomplete = {}
    for repo_id, local, required, _gated in components.assets():
        dest = MODELS_DIR / local
        missing = [name for name in required if not (dest / name).is_file()]
        if missing:
            incomplete[repo_id] = missing
    if incomplete:
        for repo_id, missing in incomplete.items():
            log(f"  incomplete {repo_id}: missing {', '.join(missing)}")
        fail("models/ is incomplete; run scripts/model-download.py first")
    log("models/ verification passed")


def models_marker():
    """Stable hash of the shipped model file list (path + size)."""
    digest = hashlib.sha256()
    entries = []
    for path in sorted(MODELS_DIR.rglob("*")):
        if path.is_file():
            entries.append((str(path.relative_to(MODELS_DIR)).replace("\\", "/"), path.stat().st_size))
    for name, size in entries:
        digest.update(f"{name}\0{size}\n".encode("utf-8"))
    return f"{digest.hexdigest()}\n"


def build_runtime(runtime_dir, python_zip, build_cache):
    """Extract embeddable Python and install every dependency, non-editable."""
    if runtime_dir.exists():
        log("Removing existing runtime/")
        shutil.rmtree(runtime_dir)
    runtime_dir.mkdir(parents=True)

    log(f"Extracting embeddable Python into {runtime_dir}")
    with zipfile.ZipFile(python_zip) as archive:
        archive.extractall(runtime_dir)

    patch_pth(runtime_dir)

    python = runtime_dir / "python.exe"
    if not python.is_file():
        fail(f"expected {python} after extracting {python_zip.name}")

    get_pip = download(GET_PIP_URL, build_cache / "get-pip.py")
    run([python, str(get_pip)])

    log("Installing build backends (isolation is unavailable under ._pth)")
    run_pip(python, ["setuptools<82", "wheel", "flit_core<4"], "build-backends")

    torch_reqs, other_reqs = split_requirements()
    log("Installing torch (CPU wheels)")
    run([python, "-m", "pip", "install", *torch_reqs, "--index-url", TORCH_CPU_INDEX])
    log("Installing runtime dependencies")
    run([python, "-m", "pip", "install", *other_reqs])

    log("Installing DiariZen (non-editable)")
    pip_install_source(python, [str(CLONE)], "diarizen")
    log("Installing bundled pyannote.audio fork (non-editable)")
    pip_install_source(
        python,
        [str(CLONE / "pyannote-audio"), "-c", str(CLONE / "constraints.txt")],
        "pyannote-audio",
    )


def patch_pth(runtime_dir):
    """Enable site + site-packages so pip-installed deps are importable."""
    matches = list(runtime_dir.glob(PTH_GLOB))
    if not matches:
        fail(f"no {PTH_GLOB} found in {runtime_dir}")
    pth = matches[0]
    lines = [line for line in pth.read_text(encoding="utf-8").splitlines()]
    wanted = ["python311.zip", ".", "Lib\\site-packages", "import site"]
    # Keep the runtime zip line if the embeddable names it differently.
    zip_line = next((line for line in lines if line.strip().endswith(".zip")), "python311.zip")
    wanted[0] = zip_line
    pth.write_text("\r\n".join(wanted) + "\r\n", encoding="utf-8")
    log(f"Patched {pth.name}: {wanted}")


def split_requirements():
    lines = (REPO_ROOT / "requirements.txt").read_text(encoding="utf-8").splitlines()
    reqs = [line.strip() for line in lines if line.strip() and not line.startswith("#")]
    torch_reqs = [r for r in reqs if r.startswith(("torch==", "torchaudio=="))]
    other_reqs = [r for r in reqs if not r.startswith(("torch==", "torchaudio=="))]
    return torch_reqs, other_reqs


def run_pip(python, args, label):
    """Run pip, capturing output to dist/build-log and echoing it on failure."""
    BUILD_LOG_DIR.mkdir(parents=True, exist_ok=True)
    log_path = BUILD_LOG_DIR / f"{label}.log"
    cmd = [str(python), "-m", "pip", "install", *[str(a) for a in args]]
    log("+ " + " ".join(cmd))
    result = subprocess.run(cmd, capture_output=True, text=True)
    log_path.write_text((result.stdout or "") + (result.stderr or ""), encoding="utf-8")
    if result.returncode != 0:
        sys.stderr.write(result.stdout or "")
        sys.stderr.write(result.stderr or "")
        fail(f"pip install failed (exit {result.returncode}); see {log_path}")
    return result


def pip_install_source(python, args, label):
    """Install a vendored source tree into the runtime.

    Build isolation cannot work from the embeddable runtime: its ``._pth`` file
    puts Python in isolated mode, which ignores the PYTHONPATH that pip's isolated
    build environment relies on. The build backends are preinstalled instead and
    isolation is disabled here.
    """
    run_pip(python, ["--no-build-isolation", *args], label)


def assemble(files=True, models=True):
    if BUNDLE.exists():
        log(f"Removing existing {BUNDLE}")
        shutil.rmtree(BUNDLE)
    (BUNDLE / "scripts").mkdir(parents=True)
    (BUNDLE / "runtime").mkdir(parents=True)

    if files:
        log("Copying scripts/")
        for name in SCRIPTS_TO_SHIP:
            source = REPO_ROOT / "scripts" / name
            if not source.is_file():
                fail(f"missing {source}")
            shutil.copy2(source, BUNDLE / "scripts" / name)

        deploy = REPO_ROOT / "deploy.bat"
        if not deploy.is_file():
            fail(f"missing {deploy}")
        shutil.copy2(deploy, BUNDLE / "deploy.bat")

        if not EXE.is_file():
            fail(f"missing {EXE}; run: cargo build --release --manifest-path gui/Cargo.toml")
        shutil.copy2(EXE, BUNDLE / "transiel-gui.exe")

    if models:
        if not MODELS_DIR.is_dir():
            fail(f"missing {MODELS_DIR}; run scripts/model-download.py first")
        verify_models()
        log("Copying models/")
        shutil.copytree(MODELS_DIR, BUNDLE / "models")
        (BUNDLE / "models" / ".complete").write_text(models_marker(), encoding="utf-8")

    version = project_version()
    (BUNDLE / "VERSION").write_text(version + "\n", encoding="utf-8")
    log(f"Wrote VERSION {version}")


def make_zip():
    if ZIP_PATH.exists():
        ZIP_PATH.unlink()
    log(f"Zipping -> {ZIP_PATH}")
    with zipfile.ZipFile(ZIP_PATH, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
        for path in sorted(BUNDLE.rglob("*")):
            if path.is_file():
                archive.write(path, arcname=str(Path("transiel") / path.relative_to(BUNDLE)))


def main():
    parser = argparse.ArgumentParser(description="Build the Windows release zip")
    parser.add_argument(
        "--python-zip",
        type=Path,
        default=None,
        help="path to a cached python-3.11-embed-amd64.zip (downloaded if absent)",
    )
    parser.add_argument(
        "--skip-runtime",
        action="store_true",
        help="assemble files/models but do not build runtime/ (dev/testing)",
    )
    parser.add_argument(
        "--no-models",
        action="store_true",
        help="do not ship models/ (smaller bundle; target downloads them)",
    )
    parser.add_argument("--zip-only", action="store_true", help="only re-zip an existing dist/transiel")
    args = parser.parse_args()

    if os.name != "nt":
        log("WARNING: this script targets Windows; the built exe/runtime will not run here")

    DIST.mkdir(parents=True, exist_ok=True)

    if not args.zip_only:
        build_cache = DIST / "build-cache"
        build_cache.mkdir(parents=True, exist_ok=True)

        assemble(files=True, models=not args.no_models)

        if not args.skip_runtime:
            if args.python_zip is not None and args.python_zip.is_file():
                python_zip = args.python_zip
            else:
                python_zip = download(EMBEDDABLE_URL, build_cache / Path(EMBEDDABLE_URL).name)
            build_runtime(BUNDLE / "runtime", python_zip, build_cache)
        else:
            log("Skipping runtime/ build (--skip-runtime)")

    make_zip()
    log(f"Done: {ZIP_PATH}")


if __name__ == "__main__":
    main()
