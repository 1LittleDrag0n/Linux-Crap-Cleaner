# Linux Crap Cleaner

A friendly, safe system cleaner for Linux, Built
with Rust, GTK4 and libadwaita.

> **Status:** early development. The Dashboard, Junk Cleaner, Storage
> Analyzer, System Leftovers, Installed Software, Duplicates & Big Files, App
> Usage and Settings all work, including system cleanups through the
> administrator helper.

## Features

| Screen | What it does | Milestone |
|---|---|---|
| Dashboard | Disk usage and a quick look at caches, package cache and logs | ✅ M1 |
| Storage Analyzer | Biggest folders and files, clickable ring chart, move to Trash | ✅ M1 |
| Junk Cleaner | Browser, app and developer caches, AUR build files, Trash, old package versions, old Snap versions, logs, crash dumps | ✅ M2 |
| System Leftovers | Orphan packages, unused Flatpak runtimes, data of removed apps, broken launchers and links, config files to review (`.pacnew`, `.rpmnew`, `.dpkg-dist`, ...) | ✅ M3 |
| Installed Software | Every system package, Flatpak, Snap and AppImage with sizes; search, sort, uninstall with a preview | ✅ M3 |
| Duplicates & Big Files | Identical files by content, very large files, empty folders; keep-one selection rules | ✅ M4 |
| App Usage | Apps not used for months (largest first), estimated or tracked | ✅ M5 |
| Administrator helper | Package cache, logs, crash dumps, orphan packages, Snap versions and uninstalling, via pkexec | ✅ M6 |
| Packaging | AppImage, built in an Ubuntu 24.04 container | ✅ M6 |

## Supported distributions

| Family | Package manager | Examples |
|---|---|---|
| Arch | pacman | Arch, CachyOS, EndeavourOS, Manjaro |
| Debian | APT (+ Snap) | Debian 13, Ubuntu 24.04+, Linux Mint 22+, Pop!_OS, elementary OS |
| Fedora | DNF (DNF 4 and 5) | Fedora 40+, RHEL/Alma/Rocky 10 |
| openSUSE | Zypper | Tumbleweed, Slowroll, Leap 16 |

Flatpak, Snap and AppImages work on all of them. Needs GTK 4.14+ and
libadwaita 1.5+. The pacman support is tested on a real system; APT, DNF,
Zypper and Snap are tested against sample output of their tools.

What differs between package managers:

- **Removal preview:** pacman and APT show exactly which packages an
  uninstall removes. DNF and Zypper can't do a dry run without root, so the
  dialog lists the package itself and says unneeded dependencies go too.
- **Package cache:** pacman keeps the newest versions of each package (set in
  Settings). APT, DNF and Zypper remove all downloaded package files.
- **Protected packages:** besides a built-in list and the running kernel,
  APT's essential/required packages and DNF's protected packages are never
  removed. On APT, the helper also does a dry run first and refuses if a
  removal would take a protected package with it.

## Safety

- Nothing is removed without a preview and your confirmation.
- Removed files go to the Trash by default. Only caches are deleted directly.
- System folders, SSH/GPG keys, keyrings and browser passwords, cookies and
  history are always protected. The full list is in Settings.
- The app never runs as root. Tasks that need root go through a small helper
  (`lcc-helper`) launched with `pkexec`, which accepts only a fixed list of
  operations.
- Packages are removed only through the package manager.

## App usage

Linux doesn't record when apps were used, and most disks are mounted with
`noatime`, so the App Usage screen estimates it from the newest change in each
app's own folders (`~/.config`, `~/.cache`, `~/.local/share`, `~/.var/app`).

For exact data, turn on **Track app usage** in Settings. That installs a small
systemd user service (`linux-crap-cleaner-usage.service`) that notes once a
minute which of your programs are running. The data stays in
`~/.local/share/linux-crap-cleaner/usage.toml`; turning the switch off removes
the service.

## The administrator helper

Some cleanups need administrator rights: the package cache, system logs,
crash dumps, orphan packages and uninstalling system packages. The app never
runs as root. Instead it starts `lcc-helper` through `pkexec`, which shows the
usual password prompt (a polkit authentication agent must be running).

The helper only accepts a short list of named tasks (see
[`privileged.rs`](crates/lcc-core/src/privileged.rs)), never a file path or a
command. It re-checks everything as root, refuses to remove critical packages
or the running kernel, and removes packages only through the system package
manager (pacman, APT, DNF, Zypper) or `snap`.

Inside the AppImage, pkexec runs the AppImage file itself with
`--lcc-helper`, and the app hands over to the bundled helper (the AppImage's
own files are mounted where only you can read them, so root can't start the
helper there directly). The password prompt then shows the AppImage's path.

During development the helper next to the app binary (`target/debug/lcc-helper`)
is used. For a nicer password prompt that remembers your password for a few
minutes, install the helper and its polkit policy:

```bash
sudo install -Dm755 target/release/lcc-helper /usr/lib/linux-crap-cleaner/lcc-helper
```

```bash
sudo install -Dm644 data/io.github.LinuxCrapCleaner.policy /usr/share/polkit-1/actions/io.github.LinuxCrapCleaner.policy
```

## AppImage

GitHub builds it for you: the [Build workflow](.github/workflows/build.yml)
tests every push and pull request on Ubuntu 24.04 and builds the AppImage
(download it from the run's **Artifacts**). Pushing a version tag publishes a
GitHub Release with the AppImage attached:

```bash
git tag v0.1.0
```

```bash
git push origin v0.1.0
```

To build it on your own computer instead:

```bash
packaging/appimage/build-in-container.sh
```

This builds `dist/Linux_Crap_Cleaner-<version>-x86_64.AppImage` inside an
Ubuntu 24.04 container (podman or docker), so the AppImage also runs on
older distributions: an AppImage only runs on systems at least as new as the
one it was built on. GTK 4 and libadwaita are bundled with
[linuxdeploy](https://github.com/linuxdeploy/linuxdeploy) and its GTK plugin,
which the script downloads into `target/appimage-tools` on first use.

`packaging/appimage/build-appimage.sh` does the same without a container (the
result then needs a system as new as yours), and `--appdir` only assembles
the AppDir without downloading anything.

## Building

Needs Rust 1.93 or newer, GTK 4.14+ and libadwaita 1.5+.

Arch / CachyOS:

```bash
sudo pacman -S --needed rustup gtk4 libadwaita
rustup default stable
```

Debian / Ubuntu:

```bash
sudo apt install cargo libgtk-4-dev libadwaita-1-dev
```

Fedora:

```bash
sudo dnf install cargo gtk4-devel libadwaita-devel
```

openSUSE:

```bash
sudo zypper install cargo gtk4-devel libadwaita-devel
```

Then build and run:

```bash
cargo run --release -p lcc-gui
```

Try the scanner and the finders from the terminal (they only look, nothing is
deleted):

```bash
cargo run --release -p lcc-core --example scan -- ~/
```

```bash
cargo run --release -p lcc-core --example junk
```

```bash
cargo run --release -p lcc-core --example leftovers
```

```bash
cargo run --release -p lcc-core --example duplicates -- ~/
```

```bash
cargo run --release -p lcc-core --example usage
```

## Adding a cleaner

Cleaners are plain rules in
[`crates/lcc-core/cleaners/builtin.toml`](crates/lcc-core/cleaners/builtin.toml).
Adding one is usually a few lines:

```toml
[[cleaner]]
id = "my-app-cache"
name = "My App"
category = "apps"
description = "Cached images. Your settings are kept."
default = true
running = ["my-app"]          # skipped while My App is open
paths = ["{cache}/my-app/*"]  # {home} {cache} {config} {data} {state}
```

Run the tests:

```bash
cargo test --workspace
```

## Project layout

```
crates/
  lcc-core/     Scanning and cleaning logic, safety rules, settings (no GUI)
    cleaners/   Junk Cleaner rules (TOML)
  lcc-gui/      The GTK4 + libadwaita app (Relm4)
  lcc-helper/   Administrator helper run through pkexec
data/           Desktop entry, icon, AppStream metadata, polkit policy
packaging/
  appimage/     AppImage build scripts and container
.github/
  workflows/    Tests, AppImage build and releases on GitHub
```

Settings are stored in `~/.config/linux-crap-cleaner/settings.toml`.

## Inspired by

BleachBit, Stacer, KDE Sweeper, Baobab, Filelight, QDirStat, Czkawka and
Flatsweep.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
