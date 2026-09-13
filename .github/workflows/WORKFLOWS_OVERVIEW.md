# GitHub Actions Workflows Overview

This document provides a quick overview of all available CI/CD workflows in this repository.

**Note:** All workflows in this repository use **manual triggers only** (`workflow_dispatch`). There are no automatic triggers from push or pull request events.

Parley is Linux-only (see [CLAUDE.md](../CLAUDE.md)) and ships
a single GPUI desktop app (`parley-gpui`) packaged as an AppImage via
`./build.sh` — there is no Tauri shell, no Next.js frontend, and no
auto-updater.

## Workflow Files

### 1. **build-linux.yml** - Linux Standalone Build
**Purpose:** One-off Linux AppImage build, run by hand

**Key Features:**
- Support for Ubuntu 22.04 and 24.04
- Builds via `./build.sh` (CPU-only — GitHub-hosted runners have no GPU)
- AppImage size/extraction/library verification
- Optional artifact upload

**Triggers:**
- Manual dispatch only

**Use When:**
- Linux-specific development
- Verifying the AppImage still bundles correctly

**Outputs:**
- `Parley-<version>-x86_64.AppImage`

---

### 2. **build-test.yml** - Multi-Platform Test Builds
**Purpose:** Test builds using the reusable `build.yml` workflow

**Key Features:**
- Uses reusable `build.yml` workflow
- Matrix over Ubuntu 22.04 / 24.04
- 30-day artifact retention

**Triggers:**
- Manual dispatch only

---

### 3. **build.yml** - Reusable Build Workflow
**Purpose:** Shared, Linux-only workflow used by other workflows

**Key Features:**
- Reusable workflow (called by others)
- Runs `./build.sh cpu` and verifies the resulting AppImage
- Used by `build-test.yml` and `release.yml`

**Not directly triggered** - used as a building block

---

### 4. **release.yml** - Production Release
**Purpose:** Create official releases with the Linux AppImage

**Key Features:**
- Creates GitHub Release (draft)
- Version comes from `parley-gpui/Cargo.toml`
- Builds the Linux AppImage (`ubuntu-22.04`) via `build.yml` and uploads it
  directly to the release
- **Auto-increment versioning**: If tag exists, auto-increments (e.g., `0.1.1` -> `0.1.1.1` -> `0.1.1.2`, up to `.100`)

**Triggers:**
- Manual dispatch only

**Use When:**
- Ready to publish a new version
- Creating official release artifacts

**Outputs:**
- GitHub Release (draft)
- Linux: `Parley-<version>-x86_64.AppImage`
- Release notes auto-generated

**Version Behavior:**
- If `v0.1.1` tag doesn't exist: creates `v0.1.1`
- If `v0.1.1` exists: creates `v0.1.1.1`
- If `v0.1.1.1` exists: creates `v0.1.1.2`
- Maximum: `v0.1.1.100` (then bump the version in `parley-gpui/Cargo.toml`)

---

### 5. **pr-main-check.yml** - Validation Check
**Purpose:** Quick validation of version and configuration

**Key Features:**
- No builds triggered
- Validates version format
- Shows current branch info
- Provides next steps guidance

**Triggers:**
- Manual dispatch only

**Use When:**
- Quick configuration check
- Before running full builds

---

## How to Run Workflows

1. **Go to Actions tab** in GitHub repository
2. **Select workflow** from left sidebar
3. **Click "Run workflow"** button
4. **Select branch** to run against
5. **Configure options** (Ubuntu version, artifact upload, etc.)
6. **Click "Run workflow"** to start
7. **Monitor progress** in the Actions tab

---

## Quick Decision Guide

### "I'm developing a new feature..."
- Build locally with `./build.sh` — it's faster than CI for iteration

### "I need to verify the AppImage still builds/packages correctly in CI..."
- **Use `build-linux.yml`** (manual dispatch)
- Choose Ubuntu version

### "I'm ready to release..."
- **Use `release.yml`** (manual dispatch)
- Creates GitHub Release
- Builds and uploads the Linux AppImage

---

## Workflow Dependencies

```
build.yml (reusable, Linux-only, CPU build via ./build.sh)
    |-- build-test.yml (matrix over Ubuntu 22.04 / 24.04)
    |-- release.yml (ubuntu-22.04 only, uploads to the release)

Standalone (don't use build.yml):
    |-- build-linux.yml
    |-- pr-main-check.yml (validation only)
```

---

## Comparison Matrix

| Workflow | Platforms | Speed | Retention | Use Case |
|----------|-----------|-------|-----------|----------|
| `build-linux.yml` | Linux | Medium | 30 days | Linux dev |
| `build-test.yml` | Linux | Medium | 30 days | Pre-release |
| `release.yml` | Linux | Medium | Permanent | Release |

---

## Required Secrets

None of the current workflows depend on repository secrets beyond the
default `GITHUB_TOKEN` (for release creation/upload). The old Tauri updater
signing keys (`TAURI_SIGNING_PRIVATE_KEY*`), license-validation key,
and Supabase secrets were removed along with the
Tauri shell.

---

## Troubleshooting

### AppImage fails to extract or is missing a library
- Check `./build.sh` ran cleanly in the workflow logs
- The `Verify AppImage` step fails loudly if `libsherpa-onnx-c-api.so` isn't
  bundled or the file is implausibly small — see `docs/building_in_linux.md`

### Artifacts not available
- Check build succeeded completely
- Artifacts expire based on retention period
- Ensure `upload-artifacts` is enabled

### Workflow not appearing in Actions
- Verify YAML syntax is valid
- Check file is in `.github/workflows/` directory
- Ensure file extension is `.yml` or `.yaml`

---

## Support

For issues with workflows:
1. Check workflow logs in Actions tab
2. Review this documentation
3. Check `ACCELERATION_GUIDE.md` for GPU/performance info
