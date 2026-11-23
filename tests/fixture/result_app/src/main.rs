mod golang;

use golang::{EmptyRequest, HashRequest};
use std::time::{Duration, Instant};

// Compile-time proof that the generated wrapper has the expected signature:
// async fn md5(...) -> Result<HashResponse, IpcError>.
#[allow(dead_code)]
fn assert_wrapper_signature() {
    let _f: fn(HashRequest) -> _ = GolangCallResult::md5;
}
use golang::GolangCallResult;

#[tokio::main]
async fn main() {
    // Go handler `md5` sleeps 4s; wrapper timeout is set to 1s via env below.
    // The wrapper must return Err at ~1s (before the Go response arrives),
    // proving the timeout converges a would-be-hanging future.
    std::env::set_var("RUST2GO_IPC_ASYNC_TIMEOUT_SECS", "1");

    let ok = GolangCallResult::ping(EmptyRequest {})
        .await
        .expect("ping must succeed");
    assert!(ok.ok, "ping response ok flag");
    println!("ping ok={}", ok.ok);

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        GolangCallResult::md5(HashRequest {
            message: "x".into(),
        }),
    )
    .await
    .expect("outer 8s guard: wrapper must converge on its own 1s timeout");
    let elapsed = started.elapsed();

    assert!(result.is_err(), "slow handler must yield Err(Timeout)");
    assert!(
        elapsed < Duration::from_secs(3),
        "wrapper must converge near 1s, took {:?}",
        elapsed
    );
    println!(
        "md5 converged via Err(Timeout) in {:?} (Go handler sleeps 4s)",
        elapsed
    );
}
