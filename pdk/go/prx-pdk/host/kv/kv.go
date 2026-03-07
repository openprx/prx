// Package kv provides isolated per-plugin key-value persistent storage.
//
// Each plugin operates in its own namespace; plugins cannot access each
// other's keys. Values are opaque byte slices. Use GetJSON / SetJSON for
// structured data (hand-written serialisation only — no reflect / encoding/json).
//
// WIT interface: prx:host/kv@0.1.0
package kv

// Get retrieves a value by key.
// Returns (nil, false) if the key does not exist.
func Get(key string) ([]byte, bool) {
	return hostKvGet(key)
}

// Set stores a byte value. Overwrites any existing value for the key.
func Set(key string, value []byte) error {
	return hostKvSet(key, value)
}

// Delete removes the key. Returns true if the key existed.
func Delete(key string) (bool, error) {
	return hostKvDelete(key)
}

// ListKeys returns all keys with the given prefix.
func ListKeys(prefix string) []string {
	return hostKvListKeys(prefix)
}

// GetString retrieves a value and decodes it as a UTF-8 string.
// Returns ("", false) if the key does not exist.
func GetString(key string) (string, bool) {
	b, ok := hostKvGet(key)
	if !ok {
		return "", false
	}
	return string(b), true
}

// SetString stores a UTF-8 string value.
func SetString(key, value string) error {
	return hostKvSet(key, []byte(value))
}

// GetJSON retrieves and JSON-decodes a stored value.
// Returns the raw JSON bytes; caller is responsible for parsing without reflect.
func GetJSON(key string) ([]byte, bool) {
	return hostKvGet(key)
}

// SetJSON encodes value as JSON (caller must provide pre-encoded bytes)
// and stores it. Use this when you have already serialised to JSON.
func SetJSON(key string, jsonBytes []byte) error {
	return hostKvSet(key, jsonBytes)
}
