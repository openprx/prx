//go:build tinygo

package log

import "unsafe"

// Canonical ABI lowering for prx:host/log@0.1.0 `log`:
//   (level: enum{0..4}) → i32
//   (message: string)   → (i32 ptr, i32 len)
//   return: void

//go:wasmimport prx:host/log@0.1.0 log
//go:noescape
func wasmLog(level int32, ptr *uint8, length int32)

func hostLog(level int32, msg string) {
	if len(msg) == 0 {
		wasmLog(level, nil, 0)
		return
	}
	b := []byte(msg)
	wasmLog(level, (*uint8)(unsafe.Pointer(&b[0])), int32(len(b)))
}
