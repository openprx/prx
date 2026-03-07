// Package pdk is the PRX Plugin Development Kit for Go / TinyGo.
//
// # Quick Start
//
//	import (
//	    pdk "github.com/openprx/prx-pdk-go"
//	    "github.com/openprx/prx-pdk-go/host/log"
//	    "github.com/openprx/prx-pdk-go/host/config"
//	    "github.com/openprx/prx-pdk-go/host/kv"
//	)
//
//	func init() {
//	    log.Info("plugin initialised")
//	    timeout := config.GetOr("timeout_ms", "5000")
//	    _ = timeout
//	    _ = pdk.OK("hello")
//	}
//
// # Build Constraints
//
// Source files tagged with `//go:build tinygo` contain the real
// WASM host-import wiring. Files tagged `//go:build !tinygo` contain
// fmt.Println stubs that allow the package to compile on the host for
// unit-testing without a WASM runtime.
//
// # TinyGo Restrictions
//
//   - No goroutines (WASM single-threaded execution model)
//   - No reflect (TinyGo limitation)
//   - No encoding/json (uses reflect internally); use manual JSON handling
//   - Only the TinyGo-compatible standard library subset
//
// Build:
//
//	tinygo build -target wasm32-wasip2 -o plugin.wasm .
package pdk

// Version of the prx-pdk-go module.
const Version = "0.1.0"

// WITPackage is the WIT package identifier for the PRX host interfaces.
const WITPackage = "prx:host@0.1.0"
