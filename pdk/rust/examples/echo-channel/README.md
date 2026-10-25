# Echo channel fixture

This local test component polls two identities, acknowledges sends, and traps on
`PRX_TRAP_PROBE`. It has no external transport. The runtime test verifies sender
filtering, recovery, generation reload and listener shutdown.

Build the checked-in component from this directory, remapping host paths:

```sh
RUSTFLAGS="--remap-path-prefix=$HOME=/prx-build" cargo build --locked --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/echo_channel.wasm plugin.wasm
```

The component is used by
`plugins::runtime::tests::wasm_channel_polls_filters_recovers_and_follows_generation_reload`.
