# rust2go-ipc-result

Companion `#[r2g_result]` attribute for [`rust2go-ipc`](https://crates.io/crates/rust2go-ipc). It generates timeout-bounded, `Result`-returning wrappers for the async methods of a `#[r2g]` trait — without modifying the upstream macro, the Go handlers, or the wire protocol.

## The problem

`#[r2g]` async methods return a bare `impl Future<Output = (Resp, (Req...))>`. When the Go side is slow, wedges, or disconnects, the IPC runtime drops the slot writer on error but does **not** wake the future. Awaiting such a call can hang forever, and with `panic = "abort"` a panic-based fallback would kill the whole process.

## The fix

`#[r2g_result]` generates a parallel `<Trait>Result` type. Each async method:

```rust
pub async fn md5(req: HashRequest) -> Result<HashResponse, rust2go::IpcError> {
    let fut = GolangCallImpl::md5(req);
    match tokio::time::timeout(rust2go::async_timeout(), fut).await {
        Ok((resp, _)) => Ok(resp),
        Err(_) => Err(rust2go::IpcError::Timeout { method: "md5", secs: ... }),
    }
}
```

On timeout the inner future is dropped, releasing the slot; the caller gets a clean `Err` instead of a hang. Business error handling (e.g. `err_msg`) is untouched — a successful response still returns `Ok(resp)`.

## Usage

```toml
[dependencies]
rust2go = { package = "rust2go-ipc", version = "0.2" }
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
    async fn md5(req: HashRequest) -> HashResponse;
}

// Call sites: switch GolangCallImpl -> GolangCallResult, add `?`
let resp = GolangCallResult::md5(req).await?;
```

`#[r2g_result]` must be placed **above** `#[r2g]`: it needs the original `async fn` signatures and re-emits the trait unchanged so `#[r2g]` still expands. Only async methods are wrapped; oneway/sync methods are left as-is.

## Configuration

| Env | Default | Range | Meaning |
|---|---|---|---|
| `RUST2GO_IPC_ASYNC_TIMEOUT_SECS` | 60 | 1..=86400 | Deadline for each generated async wrapper |

Out-of-range or unparseable values warn on stderr and fall back to the default (never panic). The value is read on each call, so it can be tuned without recompilation.

## Notes

- The generated code references `::rust2go::*` paths, resolved through the `rust2go` alias to `rust2go-ipc`. Keep the alias name `rust2go`.
- `tokio` with the `time` feature must be a dependency of the consuming crate (the generated code calls `tokio::time::timeout`). This crate itself has no tokio dependency.
- The wrapper adds a timeout only; it does not synthesize error responses or read any business fields, so it stays fully decoupled from your response schema.
