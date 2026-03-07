//go:build !tinygo

package events

import (
	"fmt"
	"os"
)

func hostEventsPublish(topic, payload string) error {
	fmt.Fprintf(os.Stderr, "[prx events stub] publish(%q, %d bytes)\n", topic, len(payload))
	return nil
}

func hostEventsSubscribe(pattern string) (uint64, error) {
	fmt.Fprintf(os.Stderr, "[prx events stub] subscribe(%q) -> id=0\n", pattern)
	return 0, nil
}

func hostEventsUnsubscribe(id uint64) error {
	fmt.Fprintf(os.Stderr, "[prx events stub] unsubscribe(%d)\n", id)
	return nil
}
