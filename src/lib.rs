//! Companion attribute for [`rust2go-ipc`](https://docs.rs/rust2go-ipc):
//! `#[r2g_result]` generates `Result`-returning wrappers for every method of a
//! `#[r2g]` trait, without touching the original macro or the business handler
//! signatures. Async methods additionally get a `tokio::time::timeout` deadline;
//! sync/oneway methods use the runtime's non-panicking dispatch path.
//!
//! # Why
//!
//! The upstream `#[r2g]` async methods return a bare
//! `impl Future<Output = (Resp, (Req...))>`. When the Go side fails (panic,
//! disconnect, TTL sweep), the runtime drops the slot writer but does **not**
//! wake the future — so an awaited call can hang forever. Raw sync calls
//! panic on transport/protocol failures because their generated signature has
//! no error channel.
//!
//! `#[r2g_result]` adds a parallel wrapper type so error convergence is
//! centralized in one place. Async wrappers return `Result<Resp, IpcError>`;
//! sync wrappers return the same for `Resp`, and oneway wrappers return
//! `Result<(), IpcError>`. No path panics on transport failure, which is safe
//! even under `panic = "abort"`.
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
//!     fn Init(req: EmptyRequest);
//!     fn newID(req: EmptyRequest) -> OrderResponse;
//!     async fn md5(req: HashRequest) -> HashResponse;
//! }
//! // generates: pub struct GolangCallResult { ... }
//! //   GolangCallResult::md5(req).await -> Result<HashResponse, IpcError>
//! //   GolangCallResult::newID(req)      -> Result<OrderResponse, IpcError>
//! //   GolangCallResult::Init(req)       -> Result<(), IpcError>
//! ```
//!
//! Async wrappers call `GolangCallImpl::<method>` (the type produced by
//! `#[r2g]`, named `<Trait>Impl`); sync/oneway wrappers call the
//! builder-generated `__r2g_try_C<Trait>_<method>` stub. Handler
//! implementations and wire behavior stay untouched. Only call sites switch
//! from `GolangCallImpl` to `GolangCallResult` and add `?`.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    parse_macro_input, parse_quote, punctuated::Punctuated, FnArg, Ident, ItemTrait, Meta, Pat,
    Path, ReturnType, Token, TraitItem,
};

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

/// Resolve the binding module the same way upstream `#[r2g]` does: a bare
/// `#[r2g(path)]` selects `path`, otherwise the default `binding` module is
/// used. `#[r2g(binding = path)]` also resolves to `binding`, matching
/// upstream's `parse_attrs` behavior.
fn binding_path(trat: &ItemTrait) -> Path {
    for attr in &trat.attrs {
        if !attr.path().is_ident("r2g") {
            continue;
        }
        if let Meta::List(list) = &attr.meta {
            if let Ok(args) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            {
                for arg in args {
                    match arg {
                        Meta::Path(p) => return p,
                        Meta::NameValue(nv) if nv.path.is_ident("binding") => return nv.path,
                        _ => {}
                    }
                }
            }
        }
    }
    parse_quote!(binding)
}

fn expand(trat: ItemTrait) -> syn::Result<proc_macro2::TokenStream> {
    let trait_ident = &trat.ident;
    let trait_span = trait_ident.span();
    let impl_ident = syn::Ident::new(&format!("{}Impl", trait_ident), trait_span);
    let result_ident = syn::Ident::new(&format!("{}Result", trait_ident), trait_span);
    let trait_vis = &trat.vis;
    let binding = binding_path(&trat);

    let mut methods = Vec::new();
    for item in &trat.items {
        let TraitItem::Fn(f) = item else { continue };

        let method = &f.sig.ident;
        let method_name = method.to_string();

        // Forward parameters exactly as declared, e.g. `req: HashRequest`.
        let inputs = &f.sig.inputs;
        let arg_names: Vec<Ident> = inputs
            .iter()
            .filter_map(|arg| match arg {
                FnArg::Typed(pat) => match &*pat.pat {
                    Pat::Ident(id) => Some(id.ident.clone()),
                    _ => None,
                },
                FnArg::Receiver(_) => None,
            })
            .collect();

        let resp_ty = match &f.sig.output {
            ReturnType::Type(_, ty) => quote! { #ty },
            ReturnType::Default if f.sig.asyncness.is_none() => quote! { () },
            ReturnType::Default => {
                return Err(syn::Error::new_spanned(
                    f,
                    "#[r2g_result]: async methods must declare a response type",
                ))
            }
        };

        if f.sig.asyncness.is_some() {
            // The declared response type is the first element of the raw
            // future's Output tuple: (Resp, (Req...)). Recover it from `-> Resp`.
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
        } else {
            // Sync/oneway wrappers call the builder-generated Result stub
            // directly. Going through `GolangCallImpl` would re-enter the old
            // panic-on-error path, which is unrecoverable under panic=abort.
            let try_stub = Ident::new(
                &format!("__r2g_try_C{}_{}", trait_ident, method),
                method.span(),
            );
            methods.push(quote! {
                pub fn #method(#inputs) -> ::core::result::Result<#resp_ty, ::rust2go::IpcError> {
                    unsafe { #binding::#try_stub(#(#arg_names),*) }
                }
            });
        }
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
        assert!(out.contains("GolangCallResult"));
        assert!(out.contains("__r2g_try_CGolangCall_md5"));
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
    fn wraps_sync_and_oneway_methods_without_touching_original_stub() {
        let out = expand_str(
            "pub trait GolangCall { fn Init(req: Req); fn newID(req: Req) -> Resp; async fn md5(req: Req) -> Resp; }",
        );
        assert!(out.contains("GolangCallImpl :: md5"));
        assert!(!out.contains("GolangCallImpl :: Init"));
        assert!(!out.contains("GolangCallImpl :: newID"));
        assert!(out.contains("binding :: __r2g_try_CGolangCall_Init"));
        assert!(out.contains("binding :: __r2g_try_CGolangCall_newID"));
        assert!(out.contains("Result < () , :: rust2go :: IpcError >"));
    }

    #[test]
    fn honors_custom_binding_path_from_r2g_attribute() {
        let out =
            expand_str("#[r2g(my_binding)] pub trait GolangCall { fn newID(req: Req) -> Resp; }");
        assert!(out.contains("my_binding :: __r2g_try_CGolangCall_newID"));
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

    #[test]
    fn preserves_r2g_include_binding_macro_item() {
        // Real-project shape: r2g_include_binding!() is a TraitItem::Macro and
        // must be re-emitted untouched for #[r2g] to process.
        let out = expand_str(
            "#[r2g] pub trait GolangCall { rust2go::r2g_include_binding!(); async fn md5(req: Req) -> Resp; }",
        );
        assert!(out.contains("r2g_include_binding !"));
        assert!(out.contains("GolangCallImpl :: md5"));
    }
}
