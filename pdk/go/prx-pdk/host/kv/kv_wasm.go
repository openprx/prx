//go:build tinygo

package kv

import (
	"errors"
	"unsafe"
)

// Canonical ABI lowering for prx:host/kv@0.1.0:
//
//   get(key: string) -> option<list<u8>>
//     option<list<u8>> flattens to (i32, i32, i32) — out-pointer 12 bytes:
//       [0:4]  uint32 — discriminant: 0=none, 1=some
//       [4:8]  uint32 — bytes pointer (valid when discriminant==1)
//       [8:12] uint32 — bytes length
//
//   set(key: string, value: list<u8>) -> result<_, string>
//   delete(key: string) -> result<bool, string>
//   list-keys(prefix: string) -> list<string>
//     result<_, string> / result<bool, string> flattens to 3 values — out-ptr 12 bytes:
//       [0:4]  uint32 — discriminant: 0=ok, 1=err
//       [4:8]  uint32 — ok value or err_ptr
//       [8:12] uint32 — err_len (when discriminant==1)
//
//   list-keys returns list<string> — out-ptr 8 bytes:
//       [0:4]  uint32 — pointer to array of (ptr,len) string records
//       [4:8]  uint32 — number of strings

//go:wasmimport prx:host/kv@0.1.0 get
//go:noescape
func wasmKvGet(retPtr unsafe.Pointer, keyPtr *uint8, keyLen uint32)

//go:wasmimport prx:host/kv@0.1.0 set
//go:noescape
func wasmKvSet(retPtr unsafe.Pointer, keyPtr *uint8, keyLen uint32, valPtr *uint8, valLen uint32)

//go:wasmimport prx:host/kv@0.1.0 delete
//go:noescape
func wasmKvDelete(retPtr unsafe.Pointer, keyPtr *uint8, keyLen uint32)

//go:wasmimport prx:host/kv@0.1.0 list-keys
//go:noescape
func wasmKvListKeys(retPtr unsafe.Pointer, prefixPtr *uint8, prefixLen uint32)

// strRecord mirrors the in-memory layout of a WIT string in a list.
type strRecord struct {
	ptr, length uint32
}

func hostKvGet(key string) ([]byte, bool) {
	var ret [12]byte
	kb := []byte(key)
	var kPtr *uint8
	kLen := uint32(len(kb))
	if kLen > 0 {
		kPtr = &kb[0]
	}
	wasmKvGet(unsafe.Pointer(&ret[0]), kPtr, kLen)

	isSome := *(*uint32)(unsafe.Pointer(&ret[0]))
	if isSome == 0 {
		return nil, false
	}
	bPtr := *(*uint32)(unsafe.Pointer(&ret[4]))
	bLen := *(*uint32)(unsafe.Pointer(&ret[8]))
	if bLen == 0 {
		return []byte{}, true
	}
	src := unsafe.Slice((*byte)(unsafe.Pointer(uintptr(bPtr))), bLen)
	out := make([]byte, bLen)
	copy(out, src)
	return out, true
}

func hostKvSet(key string, value []byte) error {
	var ret [12]byte
	kb := []byte(key)
	var kPtr *uint8
	kLen := uint32(len(kb))
	if kLen > 0 {
		kPtr = &kb[0]
	}
	var vPtr *uint8
	vLen := uint32(len(value))
	if vLen > 0 {
		vPtr = &value[0]
	}
	wasmKvSet(unsafe.Pointer(&ret[0]), kPtr, kLen, vPtr, vLen)

	disc := *(*uint32)(unsafe.Pointer(&ret[0]))
	if disc != 0 {
		errPtr := *(*uint32)(unsafe.Pointer(&ret[4]))
		errLen := *(*uint32)(unsafe.Pointer(&ret[8]))
		msg := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(errPtr))), errLen))
		return errors.New(msg)
	}
	return nil
}

func hostKvDelete(key string) (bool, error) {
	var ret [12]byte
	kb := []byte(key)
	var kPtr *uint8
	kLen := uint32(len(kb))
	if kLen > 0 {
		kPtr = &kb[0]
	}
	wasmKvDelete(unsafe.Pointer(&ret[0]), kPtr, kLen)

	disc := *(*uint32)(unsafe.Pointer(&ret[0]))
	if disc == 0 {
		existed := *(*uint32)(unsafe.Pointer(&ret[4]))
		return existed != 0, nil
	}
	errPtr := *(*uint32)(unsafe.Pointer(&ret[4]))
	errLen := *(*uint32)(unsafe.Pointer(&ret[8]))
	msg := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(errPtr))), errLen))
	return false, errors.New(msg)
}

func hostKvListKeys(prefix string) []string {
	var ret [8]byte
	pb := []byte(prefix)
	var pPtr *uint8
	pLen := uint32(len(pb))
	if pLen > 0 {
		pPtr = &pb[0]
	}
	wasmKvListKeys(unsafe.Pointer(&ret[0]), pPtr, pLen)

	listPtr := *(*uint32)(unsafe.Pointer(&ret[0]))
	listLen := *(*uint32)(unsafe.Pointer(&ret[4]))
	if listLen == 0 {
		return nil
	}
	records := unsafe.Slice((*strRecord)(unsafe.Pointer(uintptr(listPtr))), listLen)
	out := make([]string, listLen)
	for i, r := range records {
		out[i] = string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(r.ptr))), r.length))
	}
	return out
}
