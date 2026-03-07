//go:build tinygo

package memory

import (
	"errors"
	"unsafe"
)

// Canonical ABI lowering for prx:host/memory@0.1.0:
//
//   store(text: string, category: string) -> result<string, string>
//     result<string, string> flattens to (i32, i32, i32) — out-ptr 12 bytes:
//       [0:4]  uint32 — discriminant: 0=ok, 1=err
//       [4:8]  uint32 — ok_ptr or err_ptr
//       [8:12] uint32 — ok_len or err_len
//
//   recall(query: string, limit: u32) -> result<list<memory-entry>, string>
//     memory-entry record: { id: string, text: string, category: string, importance: f64 }
//     list<memory-entry> → out-ptr contains pointer + length to record array
//
//     Out-pointer layout for result<list<memory-entry>, string>:
//       [0:4]   uint32  — discriminant: 0=ok, 1=err
//       -- when ok --
//       [4:8]   uint32  — list ptr (pointer to array of memoryEntryRecord)
//       [8:12]  uint32  — list len
//       -- when err --
//       [4:8]   uint32  — error string ptr
//       [8:12]  uint32  — error string len

//go:wasmimport prx:host/memory@0.1.0 store
//go:noescape
func wasmMemoryStore(retPtr unsafe.Pointer, textPtr *uint8, textLen uint32, catPtr *uint8, catLen uint32)

//go:wasmimport prx:host/memory@0.1.0 recall
//go:noescape
func wasmMemoryRecall(retPtr unsafe.Pointer, queryPtr *uint8, queryLen uint32, limit uint32)

// memoryEntryRecord mirrors the in-memory layout of the memory-entry record
// produced by the Component Model canonical ABI for WASM32 (all lengths uint32).
type memoryEntryRecord struct {
	idPtr, idLen   uint32
	txtPtr, txtLen uint32
	catPtr, catLen uint32
	_pad           uint32 // alignment before f64
	importance     float64
}

func strPtr(s string) (*uint8, uint32) {
	b := []byte(s)
	if len(b) == 0 {
		return nil, 0
	}
	return &b[0], uint32(len(b))
}

func hostMemoryStore(text, category string) (string, error) {
	var ret [12]byte
	tPtr, tLen := strPtr(text)
	cPtr, cLen := strPtr(category)
	wasmMemoryStore(unsafe.Pointer(&ret[0]), tPtr, tLen, cPtr, cLen)

	disc := *(*uint32)(unsafe.Pointer(&ret[0]))
	sPtr := *(*uint32)(unsafe.Pointer(&ret[4]))
	sLen := *(*uint32)(unsafe.Pointer(&ret[8]))
	s := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(sPtr))), sLen))
	if disc != 0 {
		return "", errors.New(s)
	}
	return s, nil
}

func hostMemoryRecall(query string, limit uint32) ([]Entry, error) {
	var ret [12]byte
	qPtr, qLen := strPtr(query)
	wasmMemoryRecall(unsafe.Pointer(&ret[0]), qPtr, qLen, limit)

	disc := *(*uint32)(unsafe.Pointer(&ret[0]))
	p := *(*uint32)(unsafe.Pointer(&ret[4]))
	n := *(*uint32)(unsafe.Pointer(&ret[8]))

	if disc != 0 {
		msg := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(p))), n))
		return nil, errors.New(msg)
	}
	if n == 0 {
		return nil, nil
	}

	recs := unsafe.Slice((*memoryEntryRecord)(unsafe.Pointer(uintptr(p))), n)
	out := make([]Entry, n)
	for i, r := range recs {
		out[i] = Entry{
			ID:         string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(r.idPtr))), r.idLen)),
			Text:       string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(r.txtPtr))), r.txtLen)),
			Category:   string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(r.catPtr))), r.catLen)),
			Importance: r.importance,
		}
	}
	return out, nil
}
