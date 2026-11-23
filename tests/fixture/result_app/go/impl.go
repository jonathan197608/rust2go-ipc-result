package main

import "time"

type Golang struct{}

func init() { GolangCallImpl = Golang{} }

func (Golang) Init(req *EmptyRequest) {}
func (Golang) newID(req *EmptyRequest) HashResponse {
	// Same pattern as md5: outlast the 1s sync deadline so only the Result
	// path can converge the call.
	time.Sleep(4 * time.Second)
	return HashResponse{hash: "late"}
}
func (Golang) md5(req *HashRequest) HashResponse {
	// Sleep past the wrapper's 1s timeout: the response arrives too late, so
	// only the wrapper's tokio::time::timeout can converge the call (Err).
	time.Sleep(4 * time.Second)
	return HashResponse{hash: "late"}
}
func (Golang) ping(req *EmptyRequest) CommonResponse { return CommonResponse{ok: true} }
