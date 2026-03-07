//go:build tinygo

package config

import "unsafe"

// Canonical ABI lowering for prx:host/config@0.1.0:
//
//   get(key: string) -> option<string>
//     option<string> flattens to (i32, i32, i32) — 3 flat values — returned
//     via an out-pointer as the first argument (caller-allocated, 12 bytes):
//       [0:4]  uint32  — discriminant: 0=none, 1=some
//       [4:8]  uint32  — string pointer (valid only when discriminant==1)
//       [8:12] uint32  — string length  (valid only when discriminant==1)
//
//   get-all() -> list<tuple<string, string>>
//     list<...> flattens to (i32, i32) — returned via out-pointer (8 bytes):
//       [0:4]  uint32  — pointer to array of (ptr, len, ptr, len) tuples
//       [4:8]  uint32  — number of tuples in the array

//go:wasmimport prx:host/config@0.1.0 get
//go:noescape
func wasmConfigGet(retPtr unsafe.Pointer, keyPtr *uint8, keyLen uint32)

//go:wasmimport prx:host/config@0.1.0 get-all
//go:noescape
func wasmConfigGetAll(retPtr unsafe.Pointer)

func hostConfigGet(key string) (string, bool) {
	var ret [12]byte
	keyBytes := []byte(key)
	var keyPtr *uint8
	keyLen := uint32(len(keyBytes))
	if keyLen > 0 {
		keyPtr = &keyBytes[0]
	}
	wasmConfigGet(unsafe.Pointer(&ret[0]), keyPtr, keyLen)

	isSome := *(*uint32)(unsafe.Pointer(&ret[0]))
	if isSome == 0 {
		return "", false
	}
	strPtr := *(*uint32)(unsafe.Pointer(&ret[4]))
	strLen := *(*uint32)(unsafe.Pointer(&ret[8]))
	if strLen == 0 {
		return "", true
	}
	// Copy from WASM linear memory into Go heap.
	s := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(strPtr))), strLen))
	return s, true
}

// tupleRecord mirrors the in-memory layout of a WIT tuple<string, string>
// as produced by the canonical ABI: two consecutive (ptr u32, len u32) pairs.
type tupleRecord struct {
	kPtr, kLen, vPtr, vLen uint32
}

func hostConfigGetAll() [][2]string {
	var ret [8]byte
	wasmConfigGetAll(unsafe.Pointer(&ret[0]))

	listPtr := *(*uint32)(unsafe.Pointer(&ret[0]))
	listLen := *(*uint32)(unsafe.Pointer(&ret[4]))
	if listLen == 0 {
		return nil
	}

	tuples := unsafe.Slice((*tupleRecord)(unsafe.Pointer(uintptr(listPtr))), listLen)
	out := make([][2]string, listLen)
	for i, t := range tuples {
		k := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(t.kPtr))), t.kLen))
		v := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(t.vPtr))), t.vLen))
		out[i] = [2]string{k, v}
	}
	return out
}
