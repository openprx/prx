//go:build !tinygo

package memory

import (
	"fmt"
	"os"
)

func hostMemoryStore(text, category string) (string, error) {
	id := "stub-" + category + "-0"
	fmt.Fprintf(os.Stderr, "[prx memory stub] store(category=%q) -> id=%q\n", category, id)
	return id, nil
}

func hostMemoryRecall(query string, limit uint32) ([]Entry, error) {
	fmt.Fprintf(os.Stderr, "[prx memory stub] recall(%q, limit=%d) -> []\n", query, limit)
	return nil, nil
}
