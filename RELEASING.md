# Releasing

1. Set the same version in `app/Cargo.toml` and `app/tauri.conf.json` (for example `0.2.0`), merge that to `main`.
2. Tag the merge commit and push the tag:

   ```bash
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. The **Release** workflow tests, builds the installer and the portable exe, writes `SHA256SUMS.txt` and creates a **draft** release with generated notes. Versions starting with `0.` are marked as pre-release.
4. Open the draft on the Releases page, check the notes and the files, then publish it.

The workflow fails if the tag does not match the version in both files.

To build without releasing, run the **Release** workflow by hand from the Actions tab. The files are attached to the run as an artifact.

Builds are not code-signed yet, so SmartScreen warns on first run.
