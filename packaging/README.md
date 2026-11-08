# Microsoft Store packaging

`scripts/build-windows.ps1 -Msix` renders `AppxManifest.xml.template` with the
values in `identity.json`, then packs the executable and its runtime DLLs.

## Before the first submission

1. In Partner Center open **Apps and games → Hippo Transcribe → Product
   identity** (the draft submission must exist).
2. Copy the three values into `identity.json`, replacing the placeholders:
   - `Package/Identity/Name` → `packageName`
   - `Package/Identity/Publisher` → `publisher`
   - `Package/Properties/PublisherDisplayName` → `publisherDisplayName`
3. Pass `-Version` for every submission, for example `-Version 1.0.0`. The
   manifest needs four parts; the script appends `.0` to a three-part version.
   The version in the Cargo manifests is what the app reports in About, so keep
   the two in step.

The Store rejects any package whose identity does not match the reservation
character for character, so these values are not optional.

## Testing locally

The Store re-signs submitted packages, so no certificate is needed to upload.
To install a build on your own machine first, `scripts/make-test-cert.ps1`
creates a self-signed code-signing certificate, exports the `.pfx`, and trusts
it for the current user:

    .\scripts\make-test-cert.ps1
    .\scripts\build-windows.ps1 -SkipTests -NoZip -Msix -Sign `
        -CertificatePath C:\keys\hippo-test.pfx -CertificatePassword hippo-test

`-Msix` already leaves the models out of the package, so `-SkipModels` is only
needed when the zip should be slim too.

To sign a package that already exists (for example one a build left unsigned),
run the same signing step on its own:

    .\scripts\sign-msix.ps1 -Msix dist\hippo-transcribe-0.1.0-windows-x64.msix

Installing needs the certificate in the machine store, because
`Add-AppxPackage` validates in the deployment service rather than the caller's
profile. `scripts/install-msix.ps1` re-launches itself through UAC, imports the
certificate there, and installs the newest package:

    .\scripts\install-msix.ps1

The Store package does not carry the models. On first run the app shows a
download card, fetches the set the current preset needs from the pinned
mirrors, resumes a interrupted download, and verifies each file's SHA-256.
After that the app works offline; the files live in the package's private data,
which removing the package deletes. `-Fresh` additionally deletes that folder
explicitly and fails if anything is left behind. Pass `-BundleModels` to pack
the models in for an offline install instead: the package grows by several
hundred megabytes and every update re-ships them.

    .\scripts\install-msix.ps1 -Fresh

When a package will not install and the status is not `Valid`,
`scripts/check-msix.ps1` prints the manifest publisher, the certificate that
actually signed the file, whether that certificate is trusted, and the exact
command that fixes it:

    .\scripts\check-msix.ps1

Packing lives in `scripts/make-msix.ps1` and runs as its own process, so
manifest and makeappx errors can be reproduced without compiling anything:

    .\scripts\diagnose-msix.ps1 -Keep

## Vulkan builds

ggml's Vulkan shader generator is a nested CMake project, and it is where a
Vulkan build usually fails. `scripts/probe-shaders.ps1` replays only that
configure against the tree the last build produced — seconds instead of a full
compile — and writes the complete output to `dist\probe-shaders.txt`:

    .\scripts\probe-shaders.ps1

`build-windows.ps1 -Probe` does the same after setting up the compiler and
generator exactly as the build does, which is the form to trust:

    .\scripts\build-windows.ps1 -Probe

Needs one real build to have run far enough to unpack whisper.cpp first.

A successful compile is not a successful GPU path. Nothing in this repo has run
the Vulkan backend on real hardware yet, so treat `-Gpu` packages as unverified
until someone transcribes on a machine with a discrete GPU and the log reports
`ASR backend: vulkan` rather than a CPU fallback. The CPU-only build needs no
such caveat.

## Assets

`assets/` is generated — do not hand-edit. Regenerate after changing the
artwork:

    python3 scripts/make-store-assets.py
    python3 scripts/make-app-icons.py

Sources with transparent corners are used as-is and left transparent on square
assets; wide and splash tiles fill their rectangle with the identity background
colour (`#C1CAD0`). Opaque-corner artwork is still masked to its round badge.
