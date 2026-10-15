//! Code generation for enums: unit code enums and data enums.
//!
//! Every variant carries an explicit `#[store(code = N)]`; serde names and declaration order never
//! reach storage. An enum whose variants are all units is a code enum: one `INTEGER` column with
//! `CHECK (c IN (codes))`, implementing `CodeEnum`. An enum with a named-field variant is a data
//! enum: the same tag column, then each data variant's fields as sparse columns
//! `<field>_<variant>_<subfield>` inside a layout guard on the tag, which emits that variant's one
//! CHECK. Unit variants of a data enum have no columns and so no guard.

use std::collections::BTreeMap;

use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{DataEnum, DeriveInput, Fields, Ident};

use crate::{
    attrs::{self, refuse, TypeAttrs},
    fields::{columns_element, schema, Field, Generics},
};

struct Variant<'a> {
    ident: &'a Ident,
    code: i64,
    /// `None` for a unit variant.
    fields: Option<Vec<Field<'a>>>,
}

pub(crate) fn expand(input: &DeriveInput, data: &DataEnum, type_attrs: TypeAttrs) -> syn::Result<TokenStream> {
    if let Some(kind) = &type_attrs.kind {
        return Err(refuse(kind, "a kind is a struct with named fields, not an enum"));
    }
    if let Some((_, span)) = type_attrs.code {
        return Err(refuse_at(span, "an enum takes `code = N` on each variant, not on the type"));
    }
    if let Some(index) = type_attrs.indexes.first() {
        return Err(refuse(index, "`index` belongs on a kind"));
    }
    if data.variants.is_empty() {
        return Err(refuse(&input.ident, "an enum without variants has no values to store"));
    }
    let mut seen: BTreeMap<i64, &Ident> = BTreeMap::new();
    let mut variants = Vec::new();
    for variant in &data.variants {
        let code = attrs::variant_code(&variant.attrs)?;
        let fields = match &variant.fields {
            Fields::Unnamed(fields) => return Err(refuse(fields, "tuple variants cannot be stored; use named fields")),
            Fields::Unit => None,
            Fields::Named(fields) => {
                let prefix = snake_case(&variant.ident.to_string());
                Some(fields.named.iter().map(|field| Field::parse(field, &input.ident, Some(&prefix))).collect::<syn::Result<Vec<_>>>()?)
            }
        };
        let Some((code, span)) = code else {
            return Err(refuse(&variant.ident, format_args!("variant `{}` needs a stable `#[store(code = N)]`", variant.ident)));
        };
        if let Some(previous) = seen.insert(code, &variant.ident) {
            return Err(refuse_at(span, format_args!("code {code} is used by both `{previous}` and `{}`", variant.ident)));
        }
        variants.push(Variant { ident: &variant.ident, code, fields });
    }
    let codes: Vec<i64> = seen.keys().copied().collect();
    if variants.iter().all(|variant| variant.fields.is_none()) {
        Ok(code_enum(input, &variants, &codes))
    } else {
        Ok(data_enum(input, &variants, &codes))
    }
}

fn refuse_at(span: Span, what: impl std::fmt::Display) -> syn::Error {
    let error = refuse(quote!(), what);
    syn::Error::new(span, error.to_string())
}

/// `TerminalSession` becomes `terminal_session`; the same rule as `flotilla_schema::snake_case`.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (index, character) in name.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

/// Arms mapping each variant to its code.
fn code_arms(variants: &[Variant<'_>]) -> Vec<TokenStream> {
    variants
        .iter()
        .map(|variant| {
            let (ident, code) = (variant.ident, variant.code);
            match variant.fields {
                None => quote!(Self::#ident => #code),
                Some(_) => quote!(Self::#ident { .. } => #code),
            }
        })
        .collect()
}

/// Reads the tag: `None` when null, refusing an unknown code.
fn decode_tag(codes: &[i64]) -> TokenStream {
    let schema = schema();
    quote! {
        let tag: ::core::option::Option<i64> = match d.take()? {
            #schema::Value::Null => ::core::option::Option::None,
            #schema::Value::Int(code) if [#(#codes),*].contains(&code) => ::core::option::Option::Some(code),
            #schema::Value::Int(code) => return ::core::result::Result::Err(#schema::DecodeError::Code { column: ::std::string::String::new(), code }),
            other => return ::core::result::Result::Err(#schema::DecodeError::unexpected("integer", &other)),
        };
    }
}

fn code_enum(input: &DeriveInput, variants: &[Variant<'_>], codes: &[i64]) -> TokenStream {
    let schema = schema();
    let ident = &input.ident;
    let generics = Generics::new(&input.generics, []);
    let Generics { impl_generics, ty_generics, where_clause } = &generics;
    let element = columns_element(ident, &generics);
    let arms = code_arms(variants);
    let decode_tag = decode_tag(codes);
    let decode_arms = variants.iter().map(|variant| {
        let (ident, code) = (variant.ident, variant.code);
        quote!(::core::option::Option::Some(#code) => ::core::option::Option::Some(Self::#ident))
    });
    quote! {
        #[automatically_derived]
        impl #impl_generics #schema::Columns for #ident #ty_generics #where_clause {
            const TABLES: usize = 0;

            fn layout(layout: &mut #schema::Layout, name: &str, required: bool) {
                layout.column(name, #schema::ColumnType::Code(<Self as #schema::CodeEnum>::CODES.to_vec()), required);
            }

            fn encode(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                e.put(#schema::Value::Int(<Self as #schema::CodeEnum>::code(self)));
                ::core::result::Result::Ok(())
            }

            fn encode_absent(e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                e.put(#schema::Value::Null);
                ::core::result::Result::Ok(())
            }

            fn decode(d: &mut #schema::Decoder) -> ::core::result::Result<::core::option::Option<Self>, #schema::DecodeError> {
                #decode_tag
                ::core::result::Result::Ok(match tag {
                    #(#decode_arms,)*
                    _ => ::core::option::Option::None,
                })
            }

            fn decode_absent(d: &mut #schema::Decoder) -> ::core::result::Result<(), #schema::DecodeError> {
                match d.take()? {
                    #schema::Value::Null => ::core::result::Result::Ok(()),
                    other => ::core::result::Result::Err(#schema::DecodeError::unexpected("null", &other)),
                }
            }
        }

        #[automatically_derived]
        impl #impl_generics #schema::Column for #ident #ty_generics #where_clause {}

        #[automatically_derived]
        impl #impl_generics #schema::CodeEnum for #ident #ty_generics #where_clause {
            const CODES: &'static [i64] = &[#(#codes),*];

            fn code(&self) -> i64 {
                match self {
                    #(#arms,)*
                }
            }
        }

        #element
    }
}

fn data_enum(input: &DeriveInput, variants: &[Variant<'_>], codes: &[i64]) -> TokenStream {
    let schema = schema();
    let ident = &input.ident;
    let data: Vec<(&Variant<'_>, &Vec<Field<'_>>)> =
        variants.iter().filter_map(|variant| variant.fields.as_ref().map(|fields| (variant, fields))).collect();
    let all_fields = data.iter().flat_map(|(_, fields)| fields.iter());
    let generics = Generics::new(&input.generics, all_fields.clone().map(Field::bound));
    let Generics { impl_generics, ty_generics, where_clause } = &generics;
    let element = columns_element(ident, &generics);
    let tables = all_fields.clone().map(Field::tables);
    let arms = code_arms(variants);

    let guards = data.iter().map(|(variant, fields)| {
        let code = variant.code;
        let layout = fields.iter().map(|field| field.layout(&quote!(name), &quote!(true)));
        quote!(layout.guard(tag, #code, |layout| { #(#layout)* });)
    });

    // Each data variant's columns, in layout order: encoded when it is the active variant and
    // absent otherwise.
    let encode = data.iter().map(|(variant, fields)| {
        let variant_ident = variant.ident;
        let idents = fields.iter().map(|field| field.ident);
        let locals: Vec<_> = fields.iter().map(|field| &field.local).collect();
        let present = fields.iter().map(|field| {
            let local = &field.local;
            field.encode(&quote!(#local))
        });
        let absent = fields.iter().map(Field::encode_absent);
        quote! {
            if let Self::#variant_ident { #(#idents: #locals,)* } = self {
                #(#present)*
            } else {
                #(#absent)*
            }
        }
    });
    let encode_absent = all_fields.clone().map(Field::encode_absent);

    let decode_tag = decode_tag(codes);
    let unit_decode = variants.iter().filter(|variant| variant.fields.is_none()).map(|variant| {
        let (variant_ident, code) = (variant.ident, variant.code);
        quote! {
            if tag == ::core::option::Option::Some(#code) {
                value = ::core::option::Option::Some(Self::#variant_ident);
            }
        }
    });
    let data_decode = data.iter().map(|(variant, fields)| {
        let (variant_ident, code) = (variant.ident, variant.code);
        let required = fields.iter().map(Field::decode_required);
        let absent = fields.iter().map(Field::decode_absent);
        let idents = fields.iter().map(|field| field.ident);
        let locals = fields.iter().map(|field| &field.local);
        quote! {
            if tag == ::core::option::Option::Some(#code) {
                #(#required)*
                value = ::core::option::Option::Some(Self::#variant_ident { #(#idents: #locals,)* });
            } else {
                #(#absent)*
            }
        }
    });
    let decode_absent = all_fields.map(Field::decode_absent);

    quote! {
        #[automatically_derived]
        impl #impl_generics #schema::Columns for #ident #ty_generics #where_clause {
            const TABLES: usize = 0 #(+ #tables)*;

            fn layout(layout: &mut #schema::Layout, name: &str, required: bool) {
                let tag = if name.is_empty() { "tag" } else { name };
                layout.column(tag, #schema::ColumnType::Code(::std::vec![#(#codes),*]), required);
                #(#guards)*
            }

            #[allow(irrefutable_let_patterns)]
            fn encode(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                let tag: i64 = match self {
                    #(#arms,)*
                };
                e.put(#schema::Value::Int(tag));
                #(#encode)*
                ::core::result::Result::Ok(())
            }

            fn encode_absent(e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                e.put(#schema::Value::Null);
                #(#encode_absent)*
                ::core::result::Result::Ok(())
            }

            fn decode(d: &mut #schema::Decoder) -> ::core::result::Result<::core::option::Option<Self>, #schema::DecodeError> {
                #decode_tag
                let mut value: ::core::option::Option<Self> = ::core::option::Option::None;
                #(#unit_decode)*
                #(#data_decode)*
                ::core::result::Result::Ok(value)
            }

            fn decode_absent(d: &mut #schema::Decoder) -> ::core::result::Result<(), #schema::DecodeError> {
                match d.take()? {
                    #schema::Value::Null => {}
                    other => return ::core::result::Result::Err(#schema::DecodeError::unexpected("null", &other)),
                }
                #(#decode_absent)*
                ::core::result::Result::Ok(())
            }
        }

        #[automatically_derived]
        impl #impl_generics #schema::Column for #ident #ty_generics #where_clause {}

        #element
    }
}
