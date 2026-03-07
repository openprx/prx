// Package events provides fire-and-forget publish/subscribe messaging.
//
// Events flow through the PRX host for auditing and access control.
// Payloads must be valid JSON, maximum 64 KB.
//
// WIT interface: prx:host/events@0.1.0
package events

// Publish sends an event to the given topic.
// All current subscribers matching the topic receive it asynchronously.
// Returns an error if the plugin lacks the "events" permission, the payload
// exceeds 64 KB, or the topic pattern is invalid.
func Publish(topic, payload string) error {
	return hostEventsPublish(topic, payload)
}

// PublishJSON sends a pre-serialised JSON payload to a topic.
// This is an alias for Publish with a semantic hint that the caller
// has already JSON-encoded the value (reflect-free usage pattern).
func PublishJSON(topic string, jsonPayload string) error {
	return hostEventsPublish(topic, jsonPayload)
}

// Subscribe registers interest in a topic pattern.
// Supports exact match ("weather.update") and wildcard ("weather.*").
// Returns the subscription ID for use with Unsubscribe.
func Subscribe(pattern string) (uint64, error) {
	return hostEventsSubscribe(pattern)
}

// Unsubscribe cancels a subscription by ID.
func Unsubscribe(id uint64) error {
	return hostEventsUnsubscribe(id)
}
