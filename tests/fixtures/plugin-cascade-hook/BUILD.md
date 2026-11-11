# cascade-hook fixture

Test-only hook. It republishes `loop.cascade` until the host returns the
existing recursion-depth error, then stores that error in KV under
`cascade.rejected`.

## Pinned rebuild (byte-for-byte)

Toolchain (must match the committed `plugin.wasm`):

- rustc 1.99.0 (`rustc --version`)
- cargo-component 0.21.1, installed locked:
  `cargo install cargo-component --version 0.21.1 --locked`
- rustup target `wasm32-wasip1` (cargo-component 0.21.1 emits wasip1)

From this directory:

```sh
cargo component build --release
cp target/wasm32-wasip1/release/cascade_hook.wasm plugin.wasm
shasum -a 256 plugin.wasm
```

The SHA-256 of the committed `plugin.wasm` is recorded in
`plugin.wasm.sha256`. After a rebuild, `cmp` the new file against
`plugin.wasm` and confirm `shasum -a 256` matches that digest.

The host test `committed_cascade_wasm_matches_documented_digest` checks
the committed binary against that digest.
