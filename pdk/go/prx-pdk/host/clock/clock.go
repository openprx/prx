// Package clock provides time utilities for PRX WASM plugins.
//
// This package uses the Go standard library time package, which TinyGo
// implements via WASI Preview 2 clock imports when targeting wasm32-wasip2.
// No custom //go:wasmimport directives are required.
package clock

import "time"

// NowMs returns the current wall-clock time as Unix milliseconds (UTC).
func NowMs() uint64 {
	return uint64(time.Now().UnixMilli())
}

// NowSec returns the current wall-clock time as Unix seconds (UTC).
func NowSec() uint64 {
	return uint64(time.Now().Unix())
}

// Timezone returns the host timezone identifier.
//
// PRX does not currently expose a timezone host interface; this always
// returns "UTC". Timezone support is planned for a future release.
func Timezone() string {
	return "UTC"
}
