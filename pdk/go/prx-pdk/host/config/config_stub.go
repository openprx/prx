//go:build !tinygo

package config

import (
	"fmt"
	"os"
)

// hostConfigGet reads from environment variables on non-WASM builds.
func hostConfigGet(key string) (string, bool) {
	if v, ok := os.LookupEnv(key); ok {
		return v, true
	}
	fmt.Fprintf(os.Stderr, "[prx config stub] get(%q) -> none\n", key)
	return "", false
}

// hostConfigGetAll returns an empty list on non-WASM builds.
func hostConfigGetAll() [][2]string {
	fmt.Fprintln(os.Stderr, "[prx config stub] get-all() -> []")
	return nil
}
