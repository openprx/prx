// Package config provides read-only plugin configuration access.
//
// Configuration values are set by the operator in plugin.toml [config]
// and cannot be changed at runtime. For mutable persistent storage use
// the kv package.
//
// WIT interface: prx:host/config@0.1.0
package config

// Get returns the value for the given configuration key.
// Returns ("", false) if the key is not set.
func Get(key string) (string, bool) {
	return hostConfigGet(key)
}

// GetAll returns all configuration key-value pairs.
func GetAll() [][2]string {
	return hostConfigGetAll()
}

// GetOr returns the value for key, or defaultVal if the key is not set.
func GetOr(key, defaultVal string) string {
	if v, ok := hostConfigGet(key); ok {
		return v
	}
	return defaultVal
}
