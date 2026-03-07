// Package http provides outbound HTTP request capabilities for plugins.
//
// Requires the "http-outbound" permission in plugin.toml.
// All URLs are validated against the plugin's http_allowlist.
//
// WIT interface: prx:host/http-outbound@0.1.0
package http

// Response is an HTTP response from an outbound request.
type Response struct {
	Status  uint16
	Headers [][2]string
	Body    []byte
}

// BodyText returns the body as a UTF-8 string.
func (r *Response) BodyText() string { return string(r.Body) }

// Request makes an HTTP request.
//
// method is an HTTP verb ("GET", "POST", etc.).
// url must match an entry in the plugin's http_allowlist.
// headers is a list of (name, value) pairs.
// body is the optional request body; pass nil for requests without a body.
func Request(method, url string, headers [][2]string, body []byte) (*Response, error) {
	return hostHTTPRequest(method, url, headers, body)
}

// Get is a convenience wrapper for HTTP GET requests without a body.
func Get(url string, headers [][2]string) (*Response, error) {
	return hostHTTPRequest("GET", url, headers, nil)
}

// PostJSON posts the given pre-serialised JSON payload.
// Automatically adds a Content-Type: application/json header if not present.
func PostJSON(url string, headers [][2]string, jsonBody []byte) (*Response, error) {
	hasContentType := false
	for _, h := range headers {
		if equalsIgnoreCase(h[0], "content-type") {
			hasContentType = true
			break
		}
	}
	if !hasContentType {
		headers = append(headers, [2]string{"Content-Type", "application/json"})
	}
	return hostHTTPRequest("POST", url, headers, jsonBody)
}

// equalsIgnoreCase compares two ASCII strings case-insensitively
// without using bytes.EqualFold (which requires reflect on some TinyGo builds).
func equalsIgnoreCase(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		ca, cb := a[i], b[i]
		if ca >= 'A' && ca <= 'Z' {
			ca += 'a' - 'A'
		}
		if cb >= 'A' && cb <= 'Z' {
			cb += 'a' - 'A'
		}
		if ca != cb {
			return false
		}
	}
	return true
}
