# PRX 0.8.20 Release Notes

PRX 0.8.20 aligns the build, installation, release, and documentation surfaces.

## Installation

- Added a pre-built-first Unix installer at `install.sh`.
- Added a Windows PowerShell installer at `install.ps1`.
- Added exact-version installation and rollback support.
- Added fail-closed SHA-256 verification and atomic binary replacement.
- Kept onboarding and service creation explicit so installation does not handle
  credentials or start background services unexpectedly.

## Rust Toolchain

- Raised the pinned and declared Rust version to 1.97.1.
- Aligned local builds, CI, security audit, release jobs, and Docker builds.
- Updated Rust 1.97 Clippy compatibility without changing runtime defaults.
- Preserved constant-time comparison and floating-point accumulation semantics
  where mechanical lint suggestions would have changed behavior.

## Release Integrity

- Release tags must match the Cargo package version.
- Release asset names now match the installer platform mapping.
- Windows and Unix checksum sidecars use the same portable format.
- Release publication verifies asset completeness and runs the Linux binary
  before creating the GitHub Release.
