# Matrix SDK maintenance backport

Base: exact published matrix-sdk0.19.1 archive SHA2562afc652a144822f6fbe24084e08bc59cb483fd0b504396d492131dec0749396e, release commit b18166c68bb958a21f0bca8b2d8320cb53583362.

Only anymap3 migration from upstream commit9c5fdd2cf5be1b7b8b20541e56d7a109db968534 is backported: event context imports/aliases and normalized/original dependency manifests. The release Apache2 LICENSE is retained because the published crate omitted it. All other archive files remain byte-identical.

Upstream fix: https://github.com/matrix-org/matrix-rust-sdk/commit/9c5fdd2cf5be1b7b8b20541e56d7a109db968534

This is locally patched source with the unchanged real SDK version, not a new upstream release. AnyMap3 retains its maintained registry package identity. Remove the root crates.io patch and directory after reviewing a published SDK release containing the migration, regenerate the lock and rerun Matrix/security gates. Reverting remediation restores the known audit failure and is not release acceptance.
