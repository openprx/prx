// event-forwarder-hook is a PRX hook plugin that forwards events from one
// topic to another, optionally adding metadata.
//
// Build:
//
//	tinygo build -target wasm32-wasip2 -o plugin.wasm .
//
// The plugin implements the prx:plugin/hook-exports interface:
//   - on-event(event_json: string) → called when a subscribed event arrives
package main

import (
	"unsafe"

	"github.com/openprx/prx-pdk-go/host/config"
	"github.com/openprx/prx-pdk-go/host/events"
	"github.com/openprx/prx-pdk-go/host/log"
)

// ── Plugin exports ────────────────────────────────────────────────────────────

// onEvent is called by the PRX host when a matching event arrives.
// It reads the source event JSON, prepends forwarding metadata, and
// publishes to the configured target topic.
//
//go:wasmexport on-event
func onEvent(eventPtr *uint8, eventLen uint32) {
	eventJSON := wasmString(eventPtr, eventLen)
	log.Debug("on-event: received " + itoa(len(eventJSON)) + " bytes")

	targetTopic := config.GetOr("forward_topic", "events.forwarded")
	addMeta := config.GetOr("add_metadata", "false") == "true"

	var payload string
	if addMeta {
		// Wrap the original event with forwarding metadata.
		payload = `{"forwarded_by":"event-forwarder-hook","original":` + eventJSON + `}`
	} else {
		payload = eventJSON
	}

	if err := events.Publish(targetTopic, payload); err != nil {
		log.Error("failed to forward event: " + err.Error())
		return
	}
	log.Info("forwarded event to " + targetTopic)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

// wasmString converts a WASM (ptr, len) pair to a Go string.
func wasmString(ptr *uint8, length uint32) string {
	if length == 0 || ptr == nil {
		return ""
	}
	return string(unsafe.Slice(ptr, length))
}

// itoa converts a non-negative integer to a decimal string.
// Avoids fmt.Sprintf (may pull in reflect in TinyGo).
func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	buf := [20]byte{}
	i := len(buf)
	for n > 0 {
		i--
		buf[i] = byte(n%10) + '0'
		n /= 10
	}
	return string(buf[i:])
}

// main is required by TinyGo for WASM builds.
func main() {}
