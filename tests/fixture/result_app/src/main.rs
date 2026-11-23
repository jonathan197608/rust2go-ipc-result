mod golang;

use golang::{EmptyRequest, GolangCallResult, HashRequest};
use std::time::{Duration, Instant};

// Compile-time proof that the generated wrapper has the expected signature:
// async fn md5(...) -> Result<HashResponse, IpcError>.
#[allow(dead_code)]
fn assert_wrapper_signature() {
    let _f: fn(HashRequest) -> _ = GolangCallResult::md5;
}

// Mirror the real project's call-site shape: a function returning
// `Result<T, String>` (async_graphql::Error also has From<String>) and using
// `(&mut fut).await.map_err(|e| e.to_string())?`. This type-checks the exact
// conversion chain used in the migrated query.rs / mutation.rs / notice.rs.
async fn call_md5(req: HashRequest) -> Result<String, String> {
    let fut = GolangCallResult::md5(req);
    tokio::pin!(fut);
    let resp = (&mut fut).await.map_err(|e| e.to_string())?;
    Ok(resp.hash)
}

async fn call_ping_take_ok(req: EmptyRequest) -> Result<bool, String> {
    let fut = GolangCallResult::ping(req);
    tokio::pin!(fut);
    Ok((&mut fut).await.map_err(|e| e.to_string())?.ok)
}

#[tokio::main]
async fn main() {
    std::env::set_var("RUST2GO_IPC_ASYNC_TIMEOUT_SECS", "1");

    let ok = call_ping_take_ok(EmptyRequest {})
        .await
        .expect("ping must succeed");
    assert!(ok, "ping ok flag");
    println!("ping ok={}", ok);

    // Go handler `md5` sleeps 4s; wrapper timeout is 1s. Must converge to Err
    // at ~1s via IpcError -> String -> ?, well before the Go response arrives.
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(8), call_md5(HashRequest {
        message: "x".into(),
    }))
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
        "md5 converged via Err(String from Timeout) in {:?} (Go sleeps 4s)",
        elapsed
    );
}
