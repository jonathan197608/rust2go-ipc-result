# rust2go-ipc-result

Companion `#[r2g_result]` attribute for [`rust2go-ipc`](https://crates.io/crates/rust2go-ipc). It generates `Result`-returning wrappers for **all** methods of a `#[r2g]` trait — async methods also get a timeout deadline — without modifying the upstream macro, the Go handlers, or the wire protocol.

## The problem

`#[r2g]` async methods return a bare future (`Output = Resp`, or `(Resp, (Req...))` with `#[drop_safe_ret]`). When the Go side is slow, wedges, or disconnects, the IPC runtime drops the slot writer on error but does **not** wake the future. Awaiting such a call can hang forever, and with `panic = "abort"` a panic-based fallback would kill the whole process.

Raw sync calls have the same issue in a different form: connect/write/read failures, bad response ids, error statuses and the sync deadline all panic at the generated stub because upstream's signature has no error channel.

## The fix

`#[r2g_result]` generates a parallel `<Trait>Result` type:

```rust
// async: deadline via tokio::time::timeout
pub async fn md5(req: HashRequest) -> Result<HashResponse, rust2go::IpcError> { /* ... */ }

// sync with return: non-panicking IPC dispatch
pub fn newID(req: EmptyRequest) -> Result<OrderResponse, rust2go::IpcError> { /* ... */ }

// oneway: write failure is reported instead of silently dropped
pub fn Init(req: EmptyRequest) -> Result<(), rust2go::IpcError> { /* ... */ }
```

Async timeout drops the inner future (the slot is reclaimed later by the runtime's TTL sweep; the writer is not forcibly woken). Sync/oneway wrappers call builder-generated `__r2g_try_C<Trait>_<method>` stubs that use `dispatch_sync_result` / `dispatch_oneway_result` instead of the legacy panic path. Business error handling (e.g. `err_msg`) is untouched — a successful response still returns `Ok(resp)`.

The async wrapper matches upstream's output shape automatically: plain `async fn` produces bare `Resp`, while `#[drop_safe_ret] async fn` produces `(Resp, (Req...))`; reference and `#[go_pass_struct]` parameters are supported on the Result path too.

## Usage

```toml
[dependencies]
rust2go = { package = "rust2go-ipc", version = "0.2.4" }
rust2go-ipc-result = "0.2"
tokio = { version = "1", features = ["time"] }
```

```rust,ignore
use rust2go::{r2g, r2g_include_binding};
use rust2go_ipc_result::r2g_result;

#[r2g_result]   // MUST come before #[r2g]
#[r2g]
pub trait GolangCall {
    rust2go::r2g_include_binding!();
    fn Init(req: EmptyRequest);
    fn newID(req: EmptyRequest) -> OrderResponse;
    async fn md5(req: HashRequest) -> HashResponse;
}

// Call sites: switch GolangCallImpl -> GolangCallResult, add `?`
let resp = GolangCallResult::md5(req).await?;
let order = GolangCallResult::newID(req)?;
GolangCallResult::Init(req)?;
```

`#[r2g_result]` must be placed **above** `#[r2g]`: it needs the original method signatures and re-emits the trait unchanged so `#[r2g]` still expands. Async, sync-with-return, and oneway methods are all wrapped.

## Configuration

| Env | Default | Range | Meaning |
|---|---|---|---|
| `RUST2GO_IPC_ASYNC_TIMEOUT_SECS` | 600 | 1..=86400 | Deadline for each generated async wrapper |
| `RUST2GO_IPC_SYNC_TIMEOUT_SECS` | 30 | 1..=86400 | Read deadline for raw sync calls and generated sync wrappers |

Out-of-range or unparseable values warn on stderr **once per key per process** and fall back to the default (never panic) — a bad deployment value must not flood stderr on every call. Both values are read by the Rust wrappers on each call, so caller-side deadlines can be tuned without recompilation; note the Go server's `RUST2GO_IPC_SYNC_TIMEOUT_SECS`-derived backpressure wait is sampled once at server start, so changing that env requires a server restart to affect the Go side. Runtime 0.2.4+ also pre-flights async payload size against the 64 MiB protocol limit and returns `Err(IpcError::PayloadTooLarge)` instead of a poll-time panic; this requires `rust2go-ipc` 0.2.4+ (its generated `__r2g_payload_len_*` helper).

## Notes

- The generated code references `::rust2go::*` paths, resolved through the `rust2go` alias to `rust2go-ipc`. Keep the alias name `rust2go`.
- `tokio` with the `time` feature must be a dependency of the consuming crate (only async wrappers call `tokio::time::timeout`). This crate itself has no tokio dependency.
- The wrapper does not synthesize error responses or read any business fields, so it stays fully decoupled from your response schema.
- Response decode panics are still unrecoverable under `panic = "abort"`; the Result path removes transport/protocol panics, not arbitrary panics inside business decoders.
