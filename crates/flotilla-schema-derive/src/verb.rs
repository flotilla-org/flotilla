//! `#[derive(Verb)]`: a native verb's declaration from its parameter struct (#4197).
//!
//! The type carries `#[verb(name = "...", subject = "...", effect = "...")]`, optionally with
//! `invocation`, `idempotent` and `result = "kind"`; each
//! named field is a parameter, typed by its Rust type through `Param`, and refined by
//! `#[param(text)]`, `#[param(options("a", "b"))]`, `#[param(subject = "kind")]` and
//! `#[param(default = "...")]`. Each kind is an ADR 0066 address kind, checked as the declaration
//! compiles.
use proc_macro2::TokenStream;
use quote::{quote, quote_spanned};
use syn::{ext::IdentExt, Data, DeriveInput, Fields, LitStr};

struct VerbAttrs {
    name: LitStr,
    subject: LitStr,
    effect: TokenStream,
    invocation: bool,
    idempotent: bool,
    result: Option<LitStr>,
}

fn verb_attrs(input: &DeriveInput) -> syn::Result<VerbAttrs> {
    let mut attrs = input.attrs.iter().filter(|attr| attr.path().is_ident("verb"));
    let attr = attrs
        .next()
        .ok_or_else(|| syn::Error::new_spanned(&input.ident, "a verb needs #[verb(name = ..., subject = ..., effect = ...)]"))?;
    if let Some(extra) = attrs.next() {
        return Err(syn::Error::new_spanned(extra, "declare a verb in one #[verb(...)]"));
    }
    let (mut name, mut subject, mut effect, mut invocation, mut idempotent, mut result) = (None, None, None, false, false, None);
    attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("name") {
            name = Some(meta.value()?.parse::<LitStr>()?);
        } else if meta.path.is_ident("subject") {
            subject = Some(meta.value()?.parse::<LitStr>()?);
        } else if meta.path.is_ident("effect") {
            let value = meta.value()?.parse::<LitStr>()?;
            effect = Some(match value.value().as_str() {
                "read" => quote!(::flotilla_schema::VerbEffect::Read),
                "write" => quote!(::flotilla_schema::VerbEffect::Write),
                "destructive" => quote!(::flotilla_schema::VerbEffect::Destructive),
                _ => return Err(syn::Error::new_spanned(value, "effect is read, write or destructive")),
            });
        } else if meta.path.is_ident("invocation") {
            invocation = true;
        } else if meta.path.is_ident("idempotent") {
            idempotent = true;
        } else if meta.path.is_ident("result") {
            result = Some(meta.value()?.parse::<LitStr>()?);
        } else {
            return Err(meta.error("#[verb] accepts name, subject, effect, invocation, idempotent and result"));
        }
        Ok(())
    })?;
    let missing = |what| syn::Error::new_spanned(attr, format!("#[verb] needs {what}"));
    let (name, subject) = (name.ok_or_else(|| missing("name"))?, subject.ok_or_else(|| missing("subject"))?);
    if !name.value().strip_prefix(&subject.value()).is_some_and(|rest| rest.len() > 1 && rest.starts_with('.')) {
        return Err(syn::Error::new_spanned(&name, "a verb's name is `<subject kind>.<verb>`"));
    }
    Ok(VerbAttrs { name, subject, effect: effect.ok_or_else(|| missing("effect"))?, invocation, idempotent, result })
}

/// One parameter's declaration; a subject parameter's kind joins `subjects`.
fn param(field: &syn::Field, subjects: &mut Vec<LitStr>) -> syn::Result<TokenStream> {
    let name = field.ident.as_ref().expect("named field").unraw().to_string();
    let ty = &field.ty;
    let (mut text, mut options, mut subject, mut default) = (false, Vec::<LitStr>::new(), None::<LitStr>, None::<LitStr>);
    for attr in field.attrs.iter().filter(|attr| attr.path().is_ident("param")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("text") {
                text = true;
            } else if meta.path.is_ident("options") {
                let content;
                syn::parenthesized!(content in meta.input);
                options = content.parse_terminated(|input| input.parse::<LitStr>(), syn::Token![,])?.into_iter().collect();
            } else if meta.path.is_ident("subject") {
                subject = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("default") {
                default = Some(meta.value()?.parse()?);
            } else {
                return Err(meta.error("#[param] accepts text, options, subject and default"));
            }
            Ok(())
        })?;
    }
    if [text, !options.is_empty(), subject.is_some()].into_iter().filter(|set| *set).count() > 1 {
        return Err(syn::Error::new_spanned(field, "a parameter is text, an enum or a subject, not several"));
    }
    let declared = quote!(<#ty as ::flotilla_schema::Param>::TYPE);
    let mut required = quote!(<#ty as ::flotilla_schema::Param>::REQUIRED);
    let ty_tokens = if text {
        quote!({
            const _: () = assert!(matches!(#declared, ::flotilla_schema::ParamType::Str), "#[param(text)] is for String fields");
            ::flotilla_schema::ParamType::Text
        })
    } else if !options.is_empty() {
        quote!({
            const _: () = assert!(matches!(#declared, ::flotilla_schema::ParamType::Str), "#[param(options(...))] is for String fields");
            ::flotilla_schema::ParamType::Enum
        })
    } else if let Some(kind) = &subject {
        subjects.push(kind.clone());
        required = quote!(<#ty as ::flotilla_schema::SubjectParam>::REQUIRED);
        quote!(::flotilla_schema::ParamType::Subject(const { ::flotilla_schema::address_kind(#kind) }))
    } else {
        declared
    };
    let default = match &default {
        Some(default) => quote!(::core::option::Option::Some(#default)),
        None => quote!(::core::option::Option::None),
    };
    Ok(quote!(::flotilla_schema::ParamDeclaration {
        name: #name,
        ty: #ty_tokens,
        required: #required,
        default: #default,
        options: ::std::vec![#(#options),*],
    }))
}

pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    let attrs = verb_attrs(input)?;
    let mut subjects = Vec::new();
    let params = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => fields.named.iter().map(|field| param(field, &mut subjects)).collect::<syn::Result<Vec<_>>>()?,
            Fields::Unit => Vec::new(),
            Fields::Unnamed(fields) => return Err(syn::Error::new_spanned(fields, "a verb's parameters are named fields")),
        },
        _ => return Err(syn::Error::new_spanned(&input.ident, "a verb's parameters are a struct")),
    };
    let ident = &input.ident;
    let (implementation, ty, clause) = input.generics.split_for_impl();
    let VerbAttrs { name, subject, effect, invocation, idempotent, result } = attrs;
    // Item consts are evaluated even by `cargo check`, so a kind that is none fails there too.
    let checks = std::iter::once(&subject)
        .chain(&result)
        .chain(&subjects)
        .map(|kind| quote_spanned!(kind.span()=> const _: () = { ::flotilla_schema::address_kind(#kind); };));
    let checks = quote!(#(#checks)*);
    let result = match result {
        Some(kind) => quote!(::core::option::Option::Some(const { ::flotilla_schema::address_kind(#kind) })),
        None => quote!(::core::option::Option::None),
    };
    Ok(quote! {
        #checks

        impl #implementation ::flotilla_schema::Verb for #ident #ty #clause {
            fn declaration() -> ::flotilla_schema::VerbDeclaration {
                ::flotilla_schema::VerbDeclaration {
                    name: #name,
                    subject: const { ::flotilla_schema::address_kind(#subject) },
                    effect: #effect,
                    invocation: #invocation,
                    idempotent: #idempotent,
                    result: #result,
                    params: ::std::vec![#(#params),*],
                }
            }
        }
    })
}
