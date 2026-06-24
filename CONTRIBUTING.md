# Contributing

Thanks for your interest in contributing to `copycopy`!

## Supported platforms

Windows and macOS.
Linux is not supported (see the README for why), so please develop and test on Windows or macOS.

## Prerequisites

- A recent stable Rust toolchain.
- macOS: Xcode Command Line Tools, plus the permissions described in the README (Input Monitoring / Screen Recording / Automation) to run the example.

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
