#![allow(non_snake_case)]
#![allow(dead_code)]

use rust2go::{r2g, R2G};
use rust2go_ipc_result::r2g_result;

pub mod binding {
    rust2go::r2g_include_binding!();
}

#[derive(R2G, Default)]
pub struct EmptyRequest {}

#[derive(R2G, Default)]
pub struct HashRequest {
    pub message: String,
}

#[derive(R2G, Default)]
pub struct HashResponse {
    pub hash: String,
}

#[derive(R2G, Default)]
pub struct CommonResponse {
    pub err_msg: String,
    pub result: Vec<u8>,
    pub ok: bool,
}

// r2g_result must precede r2g so it sees the original `async fn` signatures.
#[r2g_result]
#[r2g]
pub trait GolangCall {
    fn Init(req: EmptyRequest);
    fn newID(req: EmptyRequest) -> HashResponse;
    async fn md5(req: HashRequest) -> HashResponse;
    #[drop_safe_ret]
    async fn ping(req: EmptyRequest) -> CommonResponse;
}
