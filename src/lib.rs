//! Companion attribute for [`rust2go-ipc`](https://docs.rs/rust2go-ipc):
//! `#[r2g_result]` generates timeout-bounded, `Result`-returning wrappers for
//! the async methods of a `#[r2g]` trait, without touching the original macro
//! or the business handler signatures.
//!
//! # Why
//!
//! The upstream `#[r2g]` async methods return a bare
//! `impl Future<Output = (Resp, (Req...))>`. When the Go side fails (panic,
//! disconnect, TTL sweep), the runtime drops the slot writer but does **not**
//! wake the future — so an awaited call can hang forever. Wrapping every call
//! site in `tokio::time::timeout` works, but is repetitive and easy to forget.
//!
//! `#[r2g_result]` adds a parallel wrapper type so error convergence is
//! centralized in one place. The wrapper returns `Result<Resp, IpcError>`;
//! timeout drops the inner future (releasing the slot) and yields `Err`, which
//! is safe even under `panic = "abort"`.
//!
//! # Usage
//!
//! ```ignore
//! use rust2go::{r2g, r2g_include_binding};
//! use rust2go_ipc_result::r2g_result;
//!
//! #[r2g_result]   // must come BEFORE #[r2g]
//! #[r2g]
//! pub trait GolangCall {
//!     rust2go::r2g_include_binding!();
//!     async fn md5(req: HashRequest) -> HashResponse;
//! }
//! // generates: pub struct GolangCallResult { ... }
//! //   with: GolangCallResult::md5(req).await -> Result<HashResponse, IpcError>
//! ```
//!
//! The generated wrapper calls `GolangCallImpl::<method>` (the type produced by
//! `#[r2g]`, named `<Trait>Impl`), so the handler implementation and all wire
//! behavior stay untouched. Only call sites switch from `GolangCallImpl` to
//! `GolangCallResult` and add `?`.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, FnArg, ItemTrait, Pat, ReturnType, TraitItem};

/// Generate a parallel `<Trait>Result` type with timeout-bounded async methods.
///
/// Must be placed above `#[r2g]` on the same trait. The original trait is
/// emitted unchanged so `#[r2g]` still expands normally.
#[proc_macro_attribute]
pub fn r2g_result(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemTrait);
    match expand(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(trat: ItemTrait) -> syn::Result<proc_macro2::TokenStream> {
    let trait_ident = &trat.ident;
    let trait_span = trait_ident.span();
    let impl_ident = syn::Ident::new(&format!("{}Impl", trait_ident), trait_span);
    let result_ident = syn::Ident::new(&format!("{}Result", trait_ident), trait_span);
    let trait_vis = &trat.vis;

    let mut methods = Vec::new();
    for item in &trat.items {
        let TraitItem::Fn(f) = item else { continue };
        // Only async methods need a timeout wrapper. oneway/sync methods keep
        // their existing behavior (sync failures still panic by contract).
        if f.sig.asyncness.is_none() {
            continue;
        }

        let method = &f.sig.ident;
        let method_name = method.to_string();

        // Forward parameters exactly as declared, e.g. `req: HashRequest`.
        let inputs = &f.sig.inputs;
        let arg_names: Vec<syn::Ident> = inputs
            .iter()
            .filter_map(|arg| match arg {
                FnArg::Typed(pat) => match &*pat.pat {
                    Pat::Ident(id) => Some(id.ident.clone()),
                    _ => None,
                },
                FnArg::Receiver(_) => None,
            })
            .collect();

        // The declared response type is the first element of the raw future's
        // Output tuple: (Resp, (Req...)). Recover it from `-> Resp`.
        let resp_ty = match &f.sig.output {
            ReturnType::Type(_, ty) => quote! { #ty },
            ReturnType::Default => {
                return Err(syn::Error::new_spanned(
                    f,
                    "#[r2g_result]: async methods must declare a response type",
                ))
            }
        };

        methods.push(quote! {
            pub async fn #method(#inputs) -> ::core::result::Result<#resp_ty, ::rust2go::IpcError> {
                let __r2g_fut = #impl_ident::#method(#(#arg_names),*);
                match ::tokio::time::timeout(::rust2go::async_timeout(), __r2g_fut).await {
                    ::core::result::Result::Ok((__r2g_resp, _)) => {
                        ::core::result::Result::Ok(__r2g_resp)
                    }
                    ::core::result::Result::Err(_) => {
                        ::core::result::Result::Err(::rust2go::IpcError::Timeout {
                            method: #method_name,
                            secs: ::rust2go::async_timeout_secs(),
                        })
                    }
                }
            }
        });
    }

    let _ = trait_vis; // trait visibility; wrapper struct is always pub
    Ok(quote! {
        // Emit the trait untouched (it still carries #[r2g] below us), then the
        // parallel wrapper. Name resolution happens after all macros expand, so
        // referencing GolangCallImpl here is valid.
        #trat

        pub struct #result_ident;

        impl #result_ident {
            #(#methods)*
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::ToTokens;

    fn expand_str(src: &str) -> String {
        let trat: ItemTrait = syn::parse_str(src).unwrap();
        expand(trat).unwrap().to_token_stream().to_string()
    }

    #[test]
    fn generates_result_struct_named_after_trait() {
        let out = expand_str("pub trait GolangCall { fn md5(req: Req) -> Resp; }");
        // Note: a plain `fn` (no async) is skipped, but the struct is still made.
        assert!(out.contains("GolangCallResult"));
    }

    #[test]
    fn wraps_async_method_with_timeout_and_result() {
        let out = expand_str("pub trait GolangCall { async fn md5(req: Req) -> Resp; }");
        assert!(out.contains("tokio :: time :: timeout"));
        assert!(out.contains("IpcError :: Timeout"));
        assert!(out.contains("GolangCallImpl :: md5"));
        assert!(out.contains("Result < Resp , :: rust2go :: IpcError >"));
    }

    #[test]
    fn skips_sync_and_oneway_methods() {
        let out = expand_str(
            "pub trait GolangCall { fn Init(req: Req); fn newID(req: Req) -> Resp; async fn md5(req: Req) -> Resp; }",
        );
        assert!(out.contains("GolangCallImpl :: md5"));
        assert!(!out.contains("GolangCallImpl :: Init"));
        assert!(!out.contains("GolangCallImpl :: newID"));
    }

    #[test]
    fn reemits_trait_so_r2g_still_expands() {
        let out = expand_str("#[r2g] pub trait GolangCall { async fn md5(req: Req) -> Resp; }");
        assert!(out.contains("# [r2g]"));
        assert!(out.contains("pub trait GolangCall"));
    }

    #[test]
    fn async_method_without_response_is_rejected() {
        let trat: ItemTrait = syn::parse_str("pub trait GolangCall { async fn ping(); }").unwrap();
        assert!(expand(trat).is_err());
    }
}
