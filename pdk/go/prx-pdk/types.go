// Package pdk provides types and convenience wrappers for PRX WASM plugins.
//
// Types are aligned with the Rust PDK and the PRX WIT interfaces at
// prx:host@0.1.0.
package pdk

// ToolSpec describes a callable tool exposed by a plugin.
// Returned from the plugin's get-spec export.
type ToolSpec struct {
	// Name is the tool identifier in snake_case.
	Name string
	// Description is a human-readable explanation shown to the LLM.
	Description string
	// ParametersSchema is a JSON Schema string describing the input parameters.
	ParametersSchema string
}

// PluginResult is the return value from a plugin's execute / run call.
type PluginResult struct {
	// Success indicates whether the operation completed successfully.
	Success bool
	// Output is the result text (may be empty on error).
	Output string
	// Error is an optional error message populated when Success is false.
	Error string
}

// OK creates a successful PluginResult with the given output.
func OK(output string) PluginResult {
	return PluginResult{Success: true, Output: output}
}

// Fail creates a failed PluginResult with the given error message.
func Fail(err string) PluginResult {
	return PluginResult{Success: false, Error: err}
}

// HttpResponse represents an HTTP response from an outbound request.
type HttpResponse struct {
	// Status is the HTTP status code.
	Status uint16
	// Headers is a list of (name, value) pairs.
	Headers [][2]string
	// Body is the raw response body bytes.
	Body []byte
}

// BodyText returns the body decoded as a UTF-8 string.
// Returns an empty string if the body is not valid UTF-8.
func (r *HttpResponse) BodyText() string {
	return string(r.Body)
}

// MiddlewareAction is the decision returned by a middleware plugin.
type MiddlewareAction uint8

const (
	// ActionContinue passes the (possibly modified) data downstream.
	ActionContinue MiddlewareAction = 0
	// ActionBlock stops processing and returns an error to the caller.
	ActionBlock MiddlewareAction = 1
)

// MiddlewareResult is the full return value from a middleware plugin.
type MiddlewareResult struct {
	// Action is the routing decision.
	Action MiddlewareAction
	// Data is the (potentially modified) JSON payload when Action == ActionContinue.
	Data string
	// Reason is the block reason when Action == ActionBlock.
	Reason string
}

// Continue returns a MiddlewareResult that passes data downstream.
func Continue(data string) MiddlewareResult {
	return MiddlewareResult{Action: ActionContinue, Data: data}
}

// Block returns a MiddlewareResult that stops processing.
func Block(reason string) MiddlewareResult {
	return MiddlewareResult{Action: ActionBlock, Reason: reason}
}

// MemoryEntry is a record returned from memory.Recall.
type MemoryEntry struct {
	// ID is the unique entry identifier.
	ID string
	// Text is the stored text content.
	Text string
	// Category is the entry category label (e.g. "fact", "preference").
	Category string
	// Importance is a relevance score in the range [0.0, 1.0].
	Importance float64
}

// CronContext carries scheduling metadata for cron plugin invocations.
// This is a PDK-level helper; it is not defined in the PRX WIT interfaces.
type CronContext struct {
	// Schedule is the cron expression that triggered this run.
	Schedule string
	// AtMs is the scheduled fire time as Unix milliseconds (UTC).
	AtMs uint64
}

// KeyValue is a string key-value pair (used by config.GetAll).
type KeyValue struct {
	Key   string
	Value string
}
