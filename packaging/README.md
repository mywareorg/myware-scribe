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
3. Bump `version` (or pass `-Version`) for every submission. The manifest needs
   four parts; the script appends `.0` to a three-part version.

The Store rejects any package whose identity does not match the reservation
character for character, so these values are not optional.

## Testing locally

The Store re-signs submitted packages, so no certificate is needed to upload.
To install a build on your own machine first, `scripts/make-test-cert.ps1`
creates a self-signed code-signing certificate, exports the `.pfx`, and trusts
it for the current user:

    .\scripts\make-test-cert.ps1
    .\scripts\build-windows.ps1 -SkipTests -SkipModels -NoZip -Msix -Sign `
        -CertificatePath C:\keys\hippo-test.pfx -CertificatePassword hippo-dev

To sign a package that already exists (for example one a build left unsigned),
run the same signing step on its own:

    .\scripts\sign-msix.ps1 -Msix dist\hippo-transcribe-0.1.0-windows-x64.msix

Installing needs the certificate in the machine store, because
`Add-AppxPackage` validates in the deployment service rather than the caller's
profile. `scripts/install-msix.ps1` re-launches itself through UAC, imports the
certificate there, and installs the newest package:

    .\scripts\install-msix.ps1

Removing a package also removes its private data, which is where downloaded
models live, so every reinstall re-downloads them. `-Fresh` additionally
deletes that folder explicitly and fails if anything is left behind:

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

## Assets

`assets/` is generated — do not hand-edit. Regenerate after changing the
artwork:

    python3 scripts/make-store-assets.py
    python3 scripts/make-app-icons.py

Square assets are masked to the round badge and left transparent; the wide
tile and splash fill their rectangle with the artwork's background colour.
