# Installing Parley

Parley runs on 64-bit (x86_64) Linux. It is distributed as an AppImage, a single file that runs on most distributions without being installed system-wide.

## Requirements

- **glibc 2.39 or newer.** Ubuntu 24.04, Debian 13, Fedora 40, Arch, or anything newer.
- **PipeWire** for audio. It is the default sound server on all of the distributions above.
- **FUSE** (`fusermount`), which AppImages use to mount themselves. Most desktops already have it; if not, install your distribution's `fuse3` package.
- About **1.5 GB of disk space** for the speech and summary models downloaded on first run, plus space for your recordings.

A GPU is optional. With an NVIDIA, AMD or Intel GPU and a working Vulkan driver, transcription runs on the GPU; otherwise it runs on the CPU.

## Install with one command

```bash
curl -fsSL https://meetparley.app/install.sh | sh
```

The installer:

1. Checks that your system meets the requirements above.
2. Downloads the latest release from GitHub. It picks the **Vulkan (GPU)** build if it finds an NVIDIA, AMD or Intel GPU with a Vulkan driver, and the **CPU** build otherwise.
3. Verifies the download's SHA-256 checksum.
4. Installs it as `~/.local/bin/parley` and adds Parley, with its icon, to your application menu.

No root access is needed. You can [read the script](https://meetparley.app/install.sh) before running it.

If `~/.local/bin` is not on your `PATH`, the installer tells you. Parley still appears in your application menu either way.

### Installer options

Set these environment variables on the `sh` side of the pipe:

| Variable | Meaning |
|---|---|
| `PARLEY_VARIANT` | `auto` (default), `vulkan` or `cpu` |
| `PARLEY_VERSION` | Install a specific version, such as `0.5.0`, instead of the latest |
| `PARLEY_BIN_DIR` | Install somewhere other than `~/.local/bin` |

For example, to force the CPU build:

```bash
curl -fsSL https://meetparley.app/install.sh | PARLEY_VARIANT=cpu sh
```

## Install by hand

1. Download an AppImage from the [latest release](https://github.com/Hankanman/Parley/releases/latest):
   - `Parley-<version>-vulkan-x86_64.AppImage` runs Whisper on your GPU. Recommended if you have one.
   - `Parley-<version>-x86_64.AppImage` is CPU only and runs anywhere.
2. Optionally check it against the matching `.sha256` file: `sha256sum -c Parley-*.AppImage.sha256`
3. Make it executable and run it:

   ```bash
   chmod +x Parley-*.AppImage
   ./Parley-*.AppImage
   ```

If FUSE isn't available and you can't install it, run the AppImage with `--appimage-extract-and-run`.

## Updating

Run the install command again; it replaces the old version and keeps your meetings and settings. To find out whether there is a new version, use **Check for updates** in the tray menu. Parley never updates itself.

## Uninstalling

```bash
curl -fsSL https://meetparley.app/install.sh | sh -s -- --uninstall
```

This removes the program, its menu entry and its icon. Your meetings, recordings, models and settings are left in place; see [Your data](data.md) to remove those too.

## Building from source

For CUDA builds, or to hack on Parley, see [Building from source](https://github.com/Hankanman/Parley/blob/main/docs/BUILDING.md) in the repository.
