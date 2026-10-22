# Releasing Parley

Parley uses [semantic versioning](https://semver.org/). There is one version
for the whole workspace, `[workspace.package] version` in the root
`Cargo.toml`; every crate inherits it, the app shows it in Settings → About,
and the in-app update check compares it with the latest GitHub release.
Release tags are `v<version>`, e.g. `v0.5.0`.

While Parley is below 1.0:

- **minor** (`0.5.0` → `0.6.0`): new features, or anything that changes
  behaviour people rely on (settings, data layout, supported setups)
- **patch** (`0.5.0` → `0.5.1`): bug fixes only

## Steps

1. Bump the version on a branch and open a PR:

   ```bash
   git checkout -b release/v0.6.0
   scripts/bump-version.sh minor      # or patch | major | 0.6.0
   git commit -am "Release v$(scripts/version.sh)"
   ```

2. Merge it to `main`.
3. Run the **Release** workflow (Actions → Release → Run workflow, on
   `main`), or `gh workflow run release.yml --ref main`. It refuses to run if
   the tag already exists. It creates a draft release `v<version>` with
   generated notes, then builds two AppImages on `ubuntu-22.04` (so they run
   on any distro with glibc 2.35+) and uploads each with a `.sha256`:
   - `Parley-<version>-x86_64.AppImage`: CPU build, runs everywhere
   - `Parley-<version>-vulkan-x86_64.AppImage`: Whisper on the GPU via
     Vulkan (NVIDIA, AMD, Intel). Compiling it needs the Vulkan SDK, not a GPU.

   The install script picks the Vulkan build on machines with an NVIDIA, AMD
   or Intel GPU and a Vulkan driver, and the CPU build otherwise. Don't
   attach AppImages built on your own machine: they need that machine's
   (usually much newer) glibc.
4. Edit the notes if needed and publish the draft. The install script
   (`curl -fsSL https://meetparley.app/install.sh | sh`) and the in-app
   update check both follow the latest published release.

`scripts/version.sh` prints the current version; `build.sh` and the
workflows use it.
