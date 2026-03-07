// Package memory provides long-term memory store and recall for plugins.
//
// Requires the "memory" permission in plugin.toml.
//
// WIT interface: prx:host/memory@0.1.0
package memory

// Entry is a memory record returned from Recall.
type Entry struct {
	ID         string
	Text       string
	Category   string
	Importance float64
}

// Store saves text in the PRX memory system.
// Returns the generated entry ID on success.
func Store(text, category string) (string, error) {
	return hostMemoryStore(text, category)
}

// Recall returns up to limit memory entries matching the query.
func Recall(query string, limit uint32) ([]Entry, error) {
	return hostMemoryRecall(query, limit)
}
