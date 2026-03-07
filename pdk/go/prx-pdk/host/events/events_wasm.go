//go:build tinygo

package events

import (
	"errors"
	"unsafe"
)

// Canonical ABI lowering for prx:host/events@0.1.0:
//
//   publish(topic: string, payload: string) -> result<_, string>
//     result<_, string> flattens to 3 values — out-ptr 12 bytes
//
//   subscribe(topic-pattern: string) -> result<u64, string>
//     result<u64, string> flattens to (i32, i64, i32) — out-ptr 16 bytes:
//       [0:4]  uint32 — discriminant: 0=ok, 1=err
//       [4:12] uint64 — subscription id (when ok)
//       or [4:8] uint32 ptr + [8:12] uint32 len (when err)
//
//   unsubscribe(subscription-id: u64) -> result<_, string>
//     same layout as publish return

//go:wasmimport prx:host/events@0.1.0 publish
//go:noescape
func wasmEventsPublish(retPtr unsafe.Pointer, topicPtr *uint8, topicLen uint32, payloadPtr *uint8, payloadLen uint32)

//go:wasmimport prx:host/events@0.1.0 subscribe
//go:noescape
func wasmEventsSubscribe(retPtr unsafe.Pointer, patternPtr *uint8, patternLen uint32)

//go:wasmimport prx:host/events@0.1.0 unsubscribe
//go:noescape
func wasmEventsUnsubscribe(retPtr unsafe.Pointer, id uint64)

func strToPtr(s string) (*uint8, uint32) {
	b := []byte(s)
	if len(b) == 0 {
		return nil, 0
	}
	return &b[0], uint32(len(b))
}

func readErrStr(ret []byte, disc uint32) error {
	if disc == 0 {
		return nil
	}
	errPtr := *(*uint32)(unsafe.Pointer(&ret[4]))
	errLen := *(*uint32)(unsafe.Pointer(&ret[8]))
	msg := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(errPtr))), errLen))
	return errors.New(msg)
}

func hostEventsPublish(topic, payload string) error {
	var ret [12]byte
	tPtr, tLen := strToPtr(topic)
	pPtr, pLen := strToPtr(payload)
	wasmEventsPublish(unsafe.Pointer(&ret[0]), tPtr, tLen, pPtr, pLen)
	return readErrStr(ret[:], *(*uint32)(unsafe.Pointer(&ret[0])))
}

func hostEventsSubscribe(pattern string) (uint64, error) {
	var ret [16]byte
	pPtr, pLen := strToPtr(pattern)
	wasmEventsSubscribe(unsafe.Pointer(&ret[0]), pPtr, pLen)

	disc := *(*uint32)(unsafe.Pointer(&ret[0]))
	if disc == 0 {
		id := *(*uint64)(unsafe.Pointer(&ret[8]))
		return id, nil
	}
	errPtr := *(*uint32)(unsafe.Pointer(&ret[8]))
	errLen := *(*uint32)(unsafe.Pointer(&ret[12]))
	msg := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(errPtr))), errLen))
	return 0, errors.New(msg)
}

func hostEventsUnsubscribe(id uint64) error {
	var ret [12]byte
	wasmEventsUnsubscribe(unsafe.Pointer(&ret[0]), id)
	return readErrStr(ret[:], *(*uint32)(unsafe.Pointer(&ret[0])))
}
