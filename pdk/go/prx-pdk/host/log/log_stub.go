//go:build !tinygo

package log

import "fmt"

// hostLog prints to stderr on non-WASM builds (stub for unit testing).
func hostLog(level int32, msg string) {
	switch level {
	case 0:
		fmt.Println("[prx TRACE]", msg)
	case 1:
		fmt.Println("[prx DEBUG]", msg)
	case 2:
		fmt.Println("[prx INFO ]", msg)
	case 3:
		fmt.Println("[prx WARN ]", msg)
	case 4:
		fmt.Println("[prx ERROR]", msg)
	default:
		fmt.Println("[prx LOG  ]", msg)
	}
}
