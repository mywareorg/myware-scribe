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

Packing lives in `scripts/make-msix.ps1` and runs as its own process, so
manifest and makeappx errors can be reproduced without compiling anything:

    .\scripts\diagnose-msix.ps1 -Keep

## Assets

`assets/` is generated — do not hand-edit. Regenerate after changing the
artwork:

    python3 scripts/make-store-assets.py

Square assets are masked to the round badge and left transparent; the wide
tile and splash fill their rectangle with the artwork's background colour.
