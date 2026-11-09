# matrix-sdk-sqlite 0.19.1 rusqlite pin

Base: published matrix-sdk-sqlite 0.19.1 from crates.io.

Only the `rusqlite` dependency is changed from `0.40.2` to `0.41` so this
crate and PRX share one `libsqlite3-sys` (`links = "sqlite3"`). libsqlite3-sys
0.38.2 is RUSTSEC-2026-0459; 0.41 pulls `>= 0.39.0`.

Remove this vendor directory and the root `[patch.crates-io]` entry after a
published matrix-sdk-sqlite release depends on rusqlite >= 0.41.
