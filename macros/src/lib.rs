extern crate proc_macro;

use core::panic;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote, ToTokens};
use syn::{
    parse_macro_input, punctuated::Punctuated, spanned::Spanned, token::Comma, FnArg, Ident,
    ImplItem, ItemFn, ItemImpl, Pat, PatType, ReturnType, Type, TypePath, Visibility,
};

// TODO: Investigate how to make the multiple spec statics be joined into a
// variable length XDR array instead of being a stream of XDR values.

#[proc_macro_attribute]
#[allow(clippy::missing_panics_doc)]
pub fn contractfn(_metadata: TokenStream, input: TokenStream) -> TokenStream {
    let func = parse_macro_input!(input as ItemFn);
    let ident = &func.sig.ident;
    let wrap_and_spec = wrap_and_spec(
        &quote! { #ident },
        ident,
        &func.sig.inputs,
        &func.sig.output,
    );
    quote! {
        #func
        #wrap_and_spec
    }
    .into()
}

#[proc_macro_attribute]
#[allow(clippy::missing_panics_doc)]
pub fn contractimpl(_metadata: TokenStream, input: TokenStream) -> TokenStream {
    let imp = parse_macro_input!(input as ItemImpl);
    let is_trait = imp.trait_.is_some();
    let self_ty = &imp.self_ty;
    let wrap_and_specs = imp
        .items
        .iter()
        .filter_map(|i| match i {
            ImplItem::Method(m) => Some(m),
            _ => None,
        })
        .filter(|m| is_trait || matches!(m.vis, Visibility::Public(_)))
        .map(|m| {
            let ident = &m.sig.ident;
            wrap_and_spec(
                &quote! { <#self_ty>::#ident },
                ident,
                &m.sig.inputs,
                &m.sig.output,
            )
        });
    quote! {
        #imp
        #(#wrap_and_specs)*
    }
    .into()
}

#[allow(clippy::too_many_lines)]
fn wrap_and_spec(
    call: &TokenStream2,
    ident: &Ident,
    inputs: &Punctuated<FnArg, Comma>,
    output: &ReturnType,
) -> TokenStream2 {
    // Collect errors as we go and emit them at the end.
    let mut errors = Vec::<TokenStream2>::new();
    errors.extend(inputs.first().and_then(|f| {
        match &f {
            FnArg::Typed(pat_type) => match &*pat_type.ty {
                Type::Path(TypePath {
                    path: syn::Path { segments, .. },
                    ..
                }) => {
                    if segments.last().map_or(false, |s| s.ident == "Env") {
                        None
                    } else {
                        Some(
                            syn::Error::new(segments.span(), "first argument must be of type Env")
                                .to_compile_error(),
                        )
                    }
                }
                _ => Some(
                    syn::Error::new(pat_type.ty.span(), "first argument must be of type Env")
                        .to_compile_error(),
                ),
            },
            FnArg::Receiver(r) => Some(
                syn::Error::new(r.span(), "first argument must be of type Env").to_compile_error(),
            ),
        }
    }));
    errors.extend(inputs.iter().skip(1).filter_map(|f| match &f {
        FnArg::Typed(pat_type) => match &*pat_type.ty {
            Type::Path(_) => None,
            _ => Some(
                syn::Error::new(pat_type.ty.span(), "argument type unsupported").to_compile_error(),
            ),
        },
        FnArg::Receiver(r) => {
            Some(syn::Error::new(r.span(), "argument type unsupported").to_compile_error())
        }
    }));
    if !errors.is_empty() {
        return quote! { #(#errors)* };
    }

    // Prepare the spec parameters.
    let spec_ident = format_ident!("__SPEC_{}", ident.to_string().to_uppercase());
    let spec_inputs = format!(
        "{:?}",
        inputs
            .iter()
            .skip(1)
            .map(|f| {
                if let &FnArg::Typed(pat_type) = &f {
                    return match &*pat_type.ty {
                        Type::Path(p) => p.into_token_stream(),
                        _ => todo!(),
                    };
                }
                panic!("only accepts functions without a self argument")
            })
            .reduce(|a, b| quote! { #a #b })
    );
    let spec_inputs_literal = proc_macro2::Literal::byte_string(spec_inputs.as_bytes());
    let spec_inputs_literal_size = spec_inputs.len();

    // Prepare the wrap parameters.
    let wrap_export_name = format!("{}", ident);
    let wrap_ident = format_ident!("__{}", ident);
    let wrap_inputs_env_ident = inputs
        .first()
        .and_then(|f| {
            if let &FnArg::Typed(pat_type) = &f {
                if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    return Some(pat_ident.ident.clone());
                }
            }
            None
        })
        .expect("only accepts functions with first parameter as the Env type");
    let wrap_inputs = inputs.iter().enumerate().map(|(i, f)| {
        if let &FnArg::Typed(pat_type) = &f {
            if i == 0 {
                return f.clone();
            }
            return FnArg::Typed(PatType {
                attrs: pat_type.attrs.clone(),
                pat: pat_type.pat.clone(),
                colon_token: pat_type.colon_token,
                ty: Box::new(Type::Verbatim(
                    TokenStream::from(quote! {
                        stellar_contract_sdk::RawVal
                    })
                    .into(),
                )),
            });
        }
        panic!("only accepts functions without a self argument")
    });
    let wrap_call_inputs = inputs.iter().skip(1).map(|f| {
        if let &FnArg::Typed(pat_type) = &f {
            if let Pat::Ident(pat_ident) = &*pat_type.pat {
                let ident = &pat_ident.ident;
                let ts: TokenStream2 = quote! {
                    <_ as stellar_contract_sdk::TryFromVal<stellar_contract_sdk::Env, stellar_contract_sdk::RawVal>>::try_from_val(&#wrap_inputs_env_ident, #ident).unwrap()
                };
                return ts;
            }
        }
        panic!("only accepts functions without a self argument")
    });

    // Output.
    match output {
        ReturnType::Default => quote! {
            #[export_name = #wrap_export_name]
            fn #wrap_ident(#(#wrap_inputs),*) -> stellar_contract_sdk::RawVal {
                #call(#(#wrap_call_inputs),*);
                stellar_contract_sdk::RawVal::from_void()
            }
            #[cfg(target_family = "wasm")]
            #[cfg_attr(target_family = "wasm", link_section = "contractspecv0")]
            pub static #spec_ident: [u8; #spec_inputs_literal_size] = *#spec_inputs_literal;
        },
        ReturnType::Type(_, _) => quote! {
            #[export_name = #wrap_export_name]
            fn #wrap_ident(#(#wrap_inputs),*) -> stellar_contract_sdk::RawVal {
                <_ as stellar_contract_sdk::IntoVal<stellar_contract_sdk::Env, stellar_contract_sdk::RawVal>>::into_val(
                    #call(
                        #wrap_inputs_env_ident.clone(),
                        #(#wrap_call_inputs),*
                    ),
                    &#wrap_inputs_env_ident
                )
            }
            #[cfg_attr(target_family = "wasm", link_section = "contractspecv0")]
            pub static #spec_ident: [u8; #spec_inputs_literal_size] = *#spec_inputs_literal;
        },
    }
}
