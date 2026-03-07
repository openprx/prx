//go:build tinygo

package http

import (
	"errors"
	"unsafe"
)

// Canonical ABI lowering for prx:host/http-outbound@0.1.0:
//
//   request(
//       method: string,
//       url: string,
//       headers: list<tuple<string,string>>,
//       body: option<list<u8>>,
//   ) -> result<http-response, string>
//
//   http-response record:
//     status: u16
//     headers: list<tuple<string,string>>
//     body: list<u8>
//
//   The function is lowered with all list arguments passed via linear memory.
//   The record return type uses an out-pointer (ret_ptr as first arg).
//
//   Out-pointer layout for result<http-response, string>:
//     [0:4]   uint32  — discriminant: 0=ok, 1=err
//     -- when ok (http-response) --
//     [4:6]   uint16  — status
//     [8:12]  uint32  — headers list ptr
//     [12:16] uint32  — headers list len
//     [16:20] uint32  — body ptr
//     [20:24] uint32  — body len
//     -- when err --
//     [4:8]   uint32  — error string ptr
//     [8:12]  uint32  — error string len

//go:wasmimport prx:host/http-outbound@0.1.0 request
//go:noescape
func wasmHTTPRequest(
	retPtr unsafe.Pointer,
	methodPtr *uint8, methodLen uint32,
	urlPtr *uint8, urlLen uint32,
	headersPtr unsafe.Pointer, headersLen uint32,
	bodyIsSome uint32, bodyPtr *uint8, bodyLen uint32,
)

// headerRecord mirrors the in-memory layout of a tuple<string, string>
// in the canonical ABI: four consecutive uint32 fields.
type headerRecord struct {
	kPtr, kLen, vPtr, vLen uint32
}

func hostHTTPRequest(method, url string, headers [][2]string, body []byte) (*Response, error) {
	var ret [32]byte

	mPtr, mLen := strPtr(method)
	uPtr, uLen := strPtr(url)

	// Build header records in Go memory for the WASM call.
	var headersPtr unsafe.Pointer
	headersLen := uint32(len(headers))
	if headersLen > 0 {
		recs := make([]headerRecord, headersLen)
		for i, h := range headers {
			kb := []byte(h[0])
			vb := []byte(h[1])
			recs[i].kLen = uint32(len(kb))
			recs[i].vLen = uint32(len(vb))
			if len(kb) > 0 {
				recs[i].kPtr = uint32(uintptr(unsafe.Pointer(&kb[0])))
			}
			if len(vb) > 0 {
				recs[i].vPtr = uint32(uintptr(unsafe.Pointer(&vb[0])))
			}
		}
		headersPtr = unsafe.Pointer(&recs[0])
	}

	var bodyIsSome uint32
	var bodyPtr *uint8
	var bodyLen uint32
	if body != nil {
		bodyIsSome = 1
		bodyLen = uint32(len(body))
		if bodyLen > 0 {
			bodyPtr = &body[0]
		}
	}

	wasmHTTPRequest(
		unsafe.Pointer(&ret[0]),
		mPtr, mLen,
		uPtr, uLen,
		headersPtr, headersLen,
		bodyIsSome, bodyPtr, bodyLen,
	)

	disc := *(*uint32)(unsafe.Pointer(&ret[0]))
	if disc != 0 {
		errPtr := *(*uint32)(unsafe.Pointer(&ret[4]))
		errLen := *(*uint32)(unsafe.Pointer(&ret[8]))
		msg := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(errPtr))), errLen))
		return nil, errors.New(msg)
	}

	resp := &Response{}
	resp.Status = *(*uint16)(unsafe.Pointer(&ret[4]))

	hListPtr := *(*uint32)(unsafe.Pointer(&ret[8]))
	hListLen := *(*uint32)(unsafe.Pointer(&ret[12]))
	if hListLen > 0 {
		recs := unsafe.Slice((*headerRecord)(unsafe.Pointer(uintptr(hListPtr))), hListLen)
		resp.Headers = make([][2]string, hListLen)
		for i, r := range recs {
			resp.Headers[i][0] = string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(r.kPtr))), r.kLen))
			resp.Headers[i][1] = string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(r.vPtr))), r.vLen))
		}
	}

	bPtr := *(*uint32)(unsafe.Pointer(&ret[16]))
	bLen := *(*uint32)(unsafe.Pointer(&ret[20]))
	if bLen > 0 {
		src := unsafe.Slice((*byte)(unsafe.Pointer(uintptr(bPtr))), bLen)
		resp.Body = make([]byte, bLen)
		copy(resp.Body, src)
	}
	return resp, nil
}

func strPtr(s string) (*uint8, uint32) {
	b := []byte(s)
	if len(b) == 0 {
		return nil, 0
	}
	return &b[0], uint32(len(b))
}
