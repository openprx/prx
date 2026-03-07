// hash-tool is a PRX tool plugin that computes SHA-256 hashes.
//
// Build:
//
//	tinygo build -target wasm32-wasip2 -o plugin.wasm .
//
// The plugin implements the prx:plugin/tool-exports interface:
//   - get-spec  → returns the tool descriptor as JSON
//   - execute   → receives {"input": "<text>"} and returns the hex hash
package main

import (
	"crypto/sha256"
	"encoding/hex"
	"unsafe"

	"github.com/openprx/prx-pdk-go/host/log"
)

// ── Plugin exports ────────────────────────────────────────────────────────────
// TinyGo maps //go:wasmexport to Component Model export functions.

//go:wasmexport get-spec
func getSpec() (ptr *uint8, length uint32) {
	log.Debug("get-spec called")
	b := []byte(toolSpecJSON())
	return &b[0], uint32(len(b))
}

//go:wasmexport execute
func execute(inputPtr *uint8, inputLen uint32) (outPtr *uint8, outLen uint32) {
	log.Info("execute called")

	input := wasmString(inputPtr, inputLen)
	text := extractStringField(input, "input")
	var result string
	if text == "" {
		result = failJSON("missing required field 'input'")
	} else {
		hash := sha256Hex(text)
		log.Info("hash computed: " + hash[:16] + "...")
		result = okJSON(hash)
	}

	b := []byte(result)
	return &b[0], uint32(len(b))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

func sha256Hex(input string) string {
	h := sha256.New()
	h.Write([]byte(input))
	return hex.EncodeToString(h.Sum(nil))
}

// wasmString converts a WASM (ptr, len) pair to a Go string.
// Uses unsafe.Slice which is available in Go ≥ 1.17 and TinyGo ≥ 0.28.
func wasmString(ptr *uint8, length uint32) string {
	if length == 0 || ptr == nil {
		return ""
	}
	return string(unsafe.Slice(ptr, length))
}

// toolSpecJSON returns the ToolSpec as a JSON string.
// Hand-built to avoid encoding/json (which uses reflect).
func toolSpecJSON() string {
	schema := `{"type":"object","properties":{"input":{"type":"string","description":"Text to hash"}},"required":["input"]}`
	return `{"name":"hash","description":"Compute SHA-256 hash of input text","parameters_schema":` +
		`"` + jsonEscape(schema) + `"` +
		`}`
}

// okJSON returns a successful PluginResult as JSON.
func okJSON(output string) string {
	return `{"success":true,"output":"` + jsonEscape(output) + `","error":null}`
}

// failJSON returns a failed PluginResult as JSON.
func failJSON(errMsg string) string {
	return `{"success":false,"output":"","error":"` + jsonEscape(errMsg) + `"}`
}

// extractStringField extracts a string value from a flat JSON object.
// Only handles top-level {"key":"value"} patterns without nesting.
// Avoids encoding/json and reflect.
func extractStringField(jsonStr, field string) string {
	needle := `"` + field + `":"`
	i := 0
	for i+len(needle) <= len(jsonStr) {
		match := true
		for j := 0; j < len(needle); j++ {
			if jsonStr[i+j] != needle[j] {
				match = false
				break
			}
		}
		if match {
			start := i + len(needle)
			end := start
			for end < len(jsonStr) {
				if jsonStr[end] == '\\' {
					end += 2
					continue
				}
				if jsonStr[end] == '"' {
					break
				}
				end++
			}
			return jsonStr[start:end]
		}
		i++
	}
	return ""
}

// jsonEscape escapes a string for safe embedding inside a JSON string literal.
func jsonEscape(s string) string {
	buf := make([]byte, 0, len(s))
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch c {
		case '"':
			buf = append(buf, '\\', '"')
		case '\\':
			buf = append(buf, '\\', '\\')
		case '\n':
			buf = append(buf, '\\', 'n')
		case '\r':
			buf = append(buf, '\\', 'r')
		case '\t':
			buf = append(buf, '\\', 't')
		default:
			buf = append(buf, c)
		}
	}
	return string(buf)
}

// main is required by TinyGo for WASM builds.
func main() {}
