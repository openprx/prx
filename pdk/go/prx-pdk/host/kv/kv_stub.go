//go:build !tinygo

package kv

import (
	"fmt"
	"os"
)

// In-process map used as a fake backing store for non-WASM tests.
var stubStore = map[string][]byte{}

func hostKvGet(key string) ([]byte, bool) {
	v, ok := stubStore[key]
	fmt.Fprintf(os.Stderr, "[prx kv stub] get(%q) -> ok=%v\n", key, ok)
	if !ok {
		return nil, false
	}
	out := make([]byte, len(v))
	copy(out, v)
	return out, true
}

func hostKvSet(key string, value []byte) error {
	fmt.Fprintf(os.Stderr, "[prx kv stub] set(%q, %d bytes)\n", key, len(value))
	v := make([]byte, len(value))
	copy(v, value)
	stubStore[key] = v
	return nil
}

func hostKvDelete(key string) (bool, error) {
	_, existed := stubStore[key]
	delete(stubStore, key)
	fmt.Fprintf(os.Stderr, "[prx kv stub] delete(%q) -> existed=%v\n", key, existed)
	return existed, nil
}

func hostKvListKeys(prefix string) []string {
	var keys []string
	for k := range stubStore {
		if len(prefix) == 0 || (len(k) >= len(prefix) && k[:len(prefix)] == prefix) {
			keys = append(keys, k)
		}
	}
	fmt.Fprintf(os.Stderr, "[prx kv stub] list-keys(%q) -> %d keys\n", prefix, len(keys))
	return keys
}
