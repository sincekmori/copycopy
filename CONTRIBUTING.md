# Contributing

Thanks for your interest in contributing to `copycopy`!

## Supported platforms

Windows, macOS, and Linux (GNOME Wayland; X11 is best-effort).
See the README's platform support table for details.

## Prerequisites

- A recent stable Rust toolchain.
- macOS: Xcode Command Line Tools, plus the permissions described in the README (Input Monitoring / Screen Recording / Automation) to run the example.
- Linux: the X11 development headers rdev needs at build time — on Debian/Ubuntu: `sudo apt-get install libx11-dev libxtst-dev libxi-dev`.

## Developing the GNOME Shell extension

The GNOME Wayland backend lives in `src/gnome/`; `extension.js` is embedded into the crate and auto-installed at runtime.
A running GNOME Shell only loads newly installed extensions at login, so iterating by logging out every time is painful.
Use a nested shell instead: `dbus-run-session -- gnome-shell --nested --wayland` starts an isolated session that loads the currently installed extension.
Run the example against the nested session bus, and copy inside the nested session to exercise it (e.g. `WAYLAND_DISPLAY=wayland-1 wl-copy "test"` twice within 400 ms).
Whenever `extension.js` changes, bump `version` in `src/gnome/metadata.json` so the installer upgrades existing installs.
When a new GNOME major is released (every March and September), append it to `shell-version` in `metadata.json` — there is no range syntax, and unlisted majors disable the extension at login.

## Before opening a PR

Please make sure the checks CI runs pass locally.

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check    # cargo install cargo-deny
typos               # cargo install typos-cli
```

## Commit messages

This project uses [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `docs:`, `refactor:`, `chore:`, `ci:`, …).
release-plz derives the changelog and version bumps from them, so please follow the convention.

## Releases

Releases are automated by release-plz.
Merging its "release" PR publishes to crates.io and tags the version, so you don't need to bump versions or edit the changelog by hand.
