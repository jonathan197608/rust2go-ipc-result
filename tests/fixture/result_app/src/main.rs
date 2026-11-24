mod golang;

use golang::{EmptyRequest, GolangCallResult, HashRequest, HashResponse};
use std::time::{Duration, Instant};

// Compile-time proof that the generated wrapper has the expected signature:
// async fn md5(...) -> Result<HashResponse, IpcError>.
#[allow(dead_code)]
fn assert_wrapper_signature() {
    let _f: fn(HashRequest) -> _ = GolangCallResult::md5;
    let _s: fn(EmptyRequest) -> Result<HashResponse, rust2go::IpcError> =
        GolangCallResult::newID;
    let _o: fn(EmptyRequest) -> Result<(), rust2go::IpcError> = GolangCallResult::Init;
}

// Mirror the real project's call-site shape: a function returning
// `Result<T, String>` (async_graphql::Error also has From<String>) and using
// `(&mut fut).await.map_err(|e| e.to_string())?`. This type-checks the exact
// conversion chain used in the migrated query.rs / mutation.rs / notice.rs.
#[allow(dead_code)] // compile-time proof of the Result<IpcError> -> Result<String> chain
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
    std::env::set_var("RUST2GO_IPC_SYNC_TIMEOUT_SECS", "1");

    // Oversized async payloads must converge on Err(PayloadTooLarge) before
    // the upstream future is polled; in panic=abort binaries a dispatch-time
    // panic would kill the process.
    let huge = HashRequest {
        message: "x".repeat(64 * 1024 * 1024 + 1),
    };
    let err = match GolangCallResult::md5(huge).await {
        Ok(_) => panic!("oversized async payload must not succeed"),
        Err(e) => e,
    };
    assert!(
        matches!(err, rust2go::IpcError::PayloadTooLarge { .. }),
        "oversized async payload must yield PayloadTooLarge, got {err}"
    );
    println!("oversized async payload converged via Err({err})");

    let ok = call_ping_take_ok(EmptyRequest {})
        .await
        .expect("ping must succeed");
    assert!(ok, "ping ok flag");
    println!("ping ok={}", ok);

    // Oneway methods now converge on Result<(), IpcError> instead of silently
    // dropping a write failure.
    GolangCallResult::Init(EmptyRequest {}).expect("oneway wrapper must return Ok");

    // Sync-with-return: Go sleeps 4s, Rust read deadline is 1s. The generated
    // wrapper must return Err(SyncTimeout) instead of panicking/aborting.
    let started = Instant::now();
    let result = GolangCallResult::newID(EmptyRequest {});
    let elapsed = started.elapsed();
    let err = result.err().expect("slow sync handler must yield Err");
    assert!(
        matches!(err, rust2go::IpcError::SyncTimeout { .. }),
        "slow sync handler must yield Err(SyncTimeout), got {err}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "sync wrapper must converge near 1s, took {:?}",
        elapsed
    );
    println!(
        "newID converged via Err(SyncTimeout) in {:?} (Go sleeps 4s)",
        elapsed
    );

    // Go handler `md5` sleeps 4s; wrapper timeout is 1s. Must converge to the
    // exact IpcError::Timeout variant at ~1s, well before the Go response
    // arrives. Assert the variant (not just is_err) so a mislabelled error
    // (e.g. Connect/Read) cannot pass.
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        GolangCallResult::md5(HashRequest { message: "x".into() }),
    )
    .await
    .expect("outer 8s guard: wrapper must converge on its own 1s timeout");
    let elapsed = started.elapsed();

    let err = result.err().expect("slow handler must yield Err");
    assert!(
        matches!(err, rust2go::IpcError::Timeout { .. }),
        "slow handler must yield Err(Timeout), got {err}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "wrapper must converge near 1s, took {:?}",
        elapsed
    );
    println!(
        "md5 converged via Err(Timeout) in {:?} (Go sleeps 4s)",
        elapsed
    );
}
