// Package log provides structured logging for PRX WASM plugins.
//
// Log messages are routed to the PRX host tracing infrastructure and
// include the plugin name as context. On non-TinyGo builds the functions
// print to stderr to allow host-side unit testing.
//
// WIT interface: prx:host/log@0.1.0
package log

// Trace emits a TRACE-level log message.
func Trace(msg string) { hostLog(0, msg) }

// Debug emits a DEBUG-level log message.
func Debug(msg string) { hostLog(1, msg) }

// Info emits an INFO-level log message.
func Info(msg string) { hostLog(2, msg) }

// Warn emits a WARN-level log message.
func Warn(msg string) { hostLog(3, msg) }

// Error emits an ERROR-level log message.
func Error(msg string) { hostLog(4, msg) }
