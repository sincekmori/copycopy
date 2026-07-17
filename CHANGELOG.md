# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.2](https://github.com/sincekmori/copycopy/compare/v0.3.1...v0.3.2) - 2026-07-17

### Other

- migrate the macOS backend to the objc2 crate family and dispatch2
- apply the Rust 2024 style edition import ordering
- commit Cargo.lock following the current Cargo guidance for libraries
- move the missing_docs lint to the Cargo.toml lints table
- add a related-crates comparison section to the README
- migrate to the Rust 2024 edition and declare the verified MSRV of 1.88

## [0.3.1](https://github.com/sincekmori/copycopy/compare/v0.3.0...v0.3.1) - 2026-07-16

### Fixed

- remove stale disabled-extensions entries and enable the extension via the running shell when possible

## [0.3.0](https://github.com/sincekmori/copycopy/compare/v0.2.1...v0.3.0) - 2026-07-16

### Added

- report trigger status to the host via start_with_status

### Other

- update the README dependency snippet from 0.2 to 0.3

## [0.2.1](https://github.com/sincekmori/copycopy/compare/v0.2.0...v0.2.1) - 2026-07-16

### Other

- update the README dependency snippet from 0.1 to 0.2

## [0.2.0](https://github.com/sincekmori/copycopy/compare/v0.1.3...v0.2.0) - 2026-07-16

### Added

- add GNOME Wayland backend via embedded Shell extension

### Other

- remove stale Linux-unsupported wording from CONTRIBUTING and the example, add a GNOME extension development guide, and add the linux crate keyword
- make release-plz bump the minor version for feat commits during 0.x so feature releases are an explicit opt-in for consumers
- ignore quick-xml build-dep advisories RUSTSEC-2026-0194/0195 in cargo-deny (xcb parses only its own bundled XML at build time and pins quick-xml 0.30)
- run clippy and tests on ubuntu-latest
- document GNOME Wayland support in README

## [0.1.3](https://github.com/sincekmori/copycopy/compare/v0.1.2...v0.1.3) - 2026-07-02

### Other

- *(deps)* bump crate-ci/typos in the actions group

## [0.1.2](https://github.com/sincekmori/copycopy/compare/v0.1.1...v0.1.2) - 2026-06-25

### Other

- single-source crate docs, dynamic license badge, and a DRY polling helper

## [0.1.1](https://github.com/sincekmori/copycopy/compare/v0.1.0...v0.1.1) - 2026-06-24

### Fixed

- *(macos)* support core-graphics 0.25
