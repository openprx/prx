//go:build !tinygo

package http

import (
	"errors"
	"fmt"
	"os"
)

func hostHTTPRequest(method, url string, headers [][2]string, body []byte) (*Response, error) {
	fmt.Fprintf(os.Stderr, "[prx http stub] %s %s (body=%d bytes)\n", method, url, len(body))
	return nil, errors.New("http::request is only available on wasm32-wasip2 targets")
}
