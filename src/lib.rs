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
    Path, ReturnType, Token, TraitItem, Type,
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
        // The generated wrapper introduces `__r2g_*` locals (e.g. `__r2g_len`,
        // `__r2g_fut`). A parameter using that prefix would be shadowed and fed
        // to the Impl call by mistake. The builder rejects the same prefix on
        // the schema side; keep the guard here too so this standalone macro
        // fails fast with a clear diagnostic even with pre-generated bindings.
        for name in &arg_names {
            if name.to_string().starts_with("__r2g_") {
                return Err(syn::Error::new_spanned(
                    name,
                    "#[r2g_result]: parameter names starting with `__r2g_` are reserved",
                ));
            }
        }
        // The pre-flight helper takes `&OwnedTy` for every parameter. Owned
        // args are borrowed; a trait param already declared `&T` is forwarded
        // as-is so the wrapper keeps ownership for the subsequent Impl call.
        let preflight_args: Vec<proc_macro2::TokenStream> = inputs
            .iter()
            .filter_map(|arg| match arg {
                FnArg::Typed(pat) => match &*pat.pat {
                    Pat::Ident(id) => {
                        let name = &id.ident;
                        Some(if matches!(&*pat.ty, Type::Reference(_)) {
                            quote! { #name }
                        } else {
                            quote! { &#name }
                        })
                    }
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
            // `dispatch` rejects >64 MiB payloads by panicking at poll time.
            // The upstream slot has no error channel, so the wrapper must
            // pre-flight the encoded size before creating the future.
            let payload_len_stub = Ident::new(
                &format!("__r2g_payload_len_C{}_{}", trait_ident, method),
                method.span(),
            );
            // Upstream #[r2g] shapes the raw future's Output as
            //   - `(Resp, (Req...))` when the method carries #[drop_safe_ret]
            //   - bare `Resp` otherwise.
            // The wrapper must destructure accordingly, or a plain async method
            // fails to compile (E0308: expected Resp, found tuple).
            // Match upstream #[r2g] exactly: it recognises these attributes
            // only as a single-segment, argument-free path via `get_ident()`.
            // Using `path().segments.last()` here would also match qualified
            // paths (`#[rust2go::drop_safe_ret]`) and `Meta::List`
            // (`#[drop_safe_ret()]`), which upstream treats as absent — that
            // mismatch makes the wrapper destructure a tuple Output the
            // upstream never produces, and omits `unsafe` on an unsafe fn.
            let drop_safe_ret = f.attrs.iter().any(|a| {
                matches!(&a.meta, Meta::Path(p)
                    if p.get_ident().is_some_and(|id| id == "drop_safe_ret"))
            });
            let drop_safe = f.attrs.iter().any(|a| {
                matches!(&a.meta, Meta::Path(p)
                    if p.get_ident().is_some_and(|id| id == "drop_safe"))
            });
            let ok_pattern = if drop_safe_ret {
                quote! { ::core::result::Result::Ok((__r2g_resp, _)) }
            } else {
                quote! { ::core::result::Result::Ok(__r2g_resp) }
            };
            // Without #[drop_safe] / #[drop_safe_ret], upstream marks the Impl
            // method `unsafe fn`, so the call needs an unsafe block. Keep the
            // safe case free of `unsafe` to avoid `unnecessary_unsafe` lints.
            let fut_expr = if drop_safe || drop_safe_ret {
                quote! { #impl_ident::#method(#(#arg_names),*) }
            } else {
                quote! { unsafe { #impl_ident::#method(#(#arg_names),*) } }
            };
            methods.push(quote! {
                pub async fn #method(#inputs) -> ::core::result::Result<#resp_ty, ::rust2go::IpcError> {
                    // The generated helper borrows every argument and encodes
                    // exactly like the async stub, so this check adds no error
                    // path inside the future (where a panic would be fatal
                    // under panic=abort).
                    let __r2g_len = unsafe {
                        #binding::#payload_len_stub(#(#preflight_args),*)
                    };
                    if __r2g_len > ::rust2go::ipc::MAX_PAYLOAD {
                        return ::core::result::Result::Err(
                            ::rust2go::IpcError::PayloadTooLarge {
                                method: #method_name.to_string(),
                                len: __r2g_len,
                            },
                        );
                    }
                    let __r2g_fut = #fut_expr;
                    // Read the env once so the reported secs always matches the
                    // deadline actually applied (no drift if the env changes
                    // between two reads on a hot-reloaded deployment).
                    let __r2g_secs = ::rust2go::async_timeout_secs();
                    let __r2g_deadline = ::core::time::Duration::from_secs(__r2g_secs);
                    match ::tokio::time::timeout(__r2g_deadline, __r2g_fut).await {
                        #ok_pattern => {
                            ::core::result::Result::Ok(__r2g_resp)
                        }
                        ::core::result::Result::Err(_) => {
                            ::core::result::Result::Err(::rust2go::IpcError::Timeout {
                                method: #method_name,
                                secs: __r2g_secs,
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
        assert!(out.contains("async_timeout_secs"));
        assert!(out.contains("IpcError :: Timeout"));
        assert!(out.contains("GolangCallImpl :: md5"));
        assert!(out.contains("Result < Resp , :: rust2go :: IpcError >"));
    }

    #[test]
    fn async_wrapper_matches_upstream_output_shape() {
        // #[drop_safe_ret] makes upstream's future Output a tuple
        // (Resp, (Req...)); without it the Output is a bare Resp. The wrapper
        // must destructure accordingly or plain async methods fail to compile.
        let with = expand_str("pub trait T { #[drop_safe_ret] async fn a(req: Req) -> Resp; }");
        assert!(
            with.contains("Ok ((__r2g_resp , _))"),
            "drop_safe_ret must destructure the (Resp, Req) tuple"
        );
        assert!(
            !with.contains("unsafe { TImpl"),
            "safe method needs no unsafe"
        );

        let without = expand_str("pub trait T { async fn a(req: Req) -> Resp; }");
        assert!(
            without.contains("Ok (__r2g_resp)"),
            "plain async must destructure a bare Resp"
        );
        assert!(
            without.contains("unsafe { TImpl :: a"),
            "non-drop-safe Impl method is unsafe fn"
        );
    }

    #[test]
    fn async_wrapper_ignores_non_bare_drop_safe_attributes() {
        // Upstream #[r2g] uses `Meta::Path(p) if p.get_ident() == Some(..)`,
        // which only matches a bare single-segment attribute. A qualified path
        // or an attribute with args is treated as ABSENT by upstream, so the
        // wrapper must do the same — otherwise it destructures a tuple Output
        // that upstream never produces and omits `unsafe` on an unsafe fn.
        let qualified =
            expand_str("pub trait T { #[rust2go::drop_safe_ret] async fn a(req: Req) -> Resp; }");
        assert!(
            qualified.contains("Ok (__r2g_resp)"),
            "qualified drop_safe_ret must NOT trigger tuple destructure"
        );
        assert!(
            qualified.contains("unsafe { TImpl :: a"),
            "qualified drop_safe_ret must NOT suppress unsafe"
        );

        let with_args =
            expand_str("pub trait T { #[drop_safe_ret()] async fn a(req: Req) -> Resp; }");
        assert!(
            with_args.contains("Ok (__r2g_resp)"),
            "drop_safe_ret() with args must NOT trigger tuple destructure"
        );
        assert!(
            with_args.contains("unsafe { TImpl :: a"),
            "drop_safe_ret() with args must NOT suppress unsafe"
        );

        // A bare attribute must still work (guards against over-correction).
        let bare = expand_str("pub trait T { #[drop_safe_ret] async fn a(req: Req) -> Resp; }");
        assert!(bare.contains("Ok ((__r2g_resp , _))"));
        assert!(!bare.contains("unsafe { TImpl"));
    }

    #[test]
    fn async_wrapper_preflights_payload_size_before_creating_future() {
        let out = expand_str("pub trait GolangCall { async fn upload(req: Req) -> Resp; }");
        assert!(
            out.contains("binding :: __r2g_payload_len_CGolangCall_upload"),
            "async wrapper must call the generated length helper"
        );
        assert!(
            out.contains("rust2go :: ipc :: MAX_PAYLOAD"),
            "oversized async payload must be checked against the protocol limit"
        );
        assert!(
            out.contains("IpcError :: PayloadTooLarge"),
            "oversized async payload must converge on a Result error"
        );
        // The check must happen before the upstream future is constructed.
        let len_pos = out
            .find("__r2g_payload_len_CGolangCall_upload")
            .expect("length helper call");
        let fut_pos = out.find("GolangCallImpl :: upload").expect("future call");
        assert!(len_pos < fut_pos, "payload check must precede Impl call");
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
    fn rejects_reserved_r2g_prefixed_parameter() {
        // The wrapper emits `__r2g_*` locals; a parameter with that prefix
        // would be shadowed and forwarded to the Impl call by mistake.
        let trat: ItemTrait =
            syn::parse_str("pub trait T { fn f(__r2g_len: Req) -> Resp; }").unwrap();
        let err = expand(trat).expect_err("reserved parameter must be rejected");
        assert!(
            err.to_string().contains("__r2g_"),
            "diagnostic must name the reserved prefix, got: {err}"
        );
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
