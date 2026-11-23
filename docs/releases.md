# Releases

Releases are built by `.github/workflows/release.yml`. Bump the `[package]
version` in `Cargo.toml`, commit, and push a tag that matches it:

```bash
git tag v0.4.11
git push origin v0.4.11
```

The workflow builds and smoke-tests `ytui-x86_64-pc-windows-msvc.exe`,
`ytui-x86_64-unknown-linux-gnu`, `ytui-aarch64-apple-darwin` and
`ytui-x86_64-apple-darwin`, publishes them with `SHA256SUMS`, and fails when
the tag does not match `Cargo.toml`. If one platform's builder fails, the
release still gets the other assets and the run is marked failed; the installer
falls back to a source build on the missing platform. Running the workflow by
hand (`workflow_dispatch`) builds the same assets without publishing a
release, so the installers can be tested before a tag exists.
