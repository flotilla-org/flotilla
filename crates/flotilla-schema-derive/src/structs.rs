//! Code generation for structs with named fields: kinds and parts.
//!
//! A kind (`#[store(kind = "Name", code = N)]`) is the root row of its own tables and implements
//! `Element` and `Kind`. A part implements `Element` (it can be a collection's row) and, having no
//! table fields, `Columns` and `Flat` (it can be flattened into another row).

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{ext::IdentExt, DeriveInput, FieldsNamed, Ident, LitStr, Type};

use crate::{
    attrs::{self, not_yet, refuse, TypeAttrs},
    shape::{self, FieldShape},
};

struct Field<'a> {
    ident: &'a Ident,
    /// A local for the decoded value that cannot shadow the generated code's own bindings.
    local: Ident,
    /// The column name: the Rust field name, never a serde name.
    name: LitStr,
    ty: &'a Type,
    shape: FieldShape,
}

pub(crate) fn expand(input: &DeriveInput, fields: &FieldsNamed, type_attrs: TypeAttrs) -> syn::Result<TokenStream> {
    let fields = fields
        .named
        .iter()
        .map(|field| {
            let ident = field.ident.as_ref().expect("named field");
            let attr = attrs::field_attr(&field.attrs)?;
            let shape = shape::classify(attr, &field.ty, &input.ident)?;
            Ok(Field {
                ident,
                local: format_ident!("__store_{}", ident.unraw()),
                name: LitStr::new(&ident.unraw().to_string(), ident.span()),
                ty: &field.ty,
                shape,
            })
        })
        .collect::<syn::Result<Vec<_>>>()?;
    for field in &fields {
        if field.shape != FieldShape::Column {
            return Err(not_yet(field.ty, field.shape.describe()));
        }
    }
    if let Some(index) = type_attrs.indexes.first() {
        return Err(not_yet(index, "`index`"));
    }
    match (&type_attrs.kind, type_attrs.code) {
        (Some(kind), Some((code, _))) => {
            check_kind_name(kind)?;
            Ok(kind_impls(input, &fields, kind, code))
        }
        (Some(kind), None) => Err(syn::Error::new_spanned(kind, "a kind needs a stable `code = N`")),
        (None, Some((_, span))) => Err(syn::Error::new(span, "`code` on a struct needs `kind = \"Name\"`")),
        (None, None) => Ok(part_impls(input, &fields)),
    }
}

fn check_kind_name(kind: &LitStr) -> syn::Result<()> {
    let name = kind.value();
    let mut chars = name.chars();
    let valid = chars.next().is_some_and(|first| first.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(syn::Error::new_spanned(kind, "a kind name is UpperCamelCase ASCII, such as \"TerminalSession\""))
    }
}

fn schema() -> TokenStream {
    quote!(::flotilla_schema)
}

/// `impl` header pieces with one `FieldTy: Column` bound per field, so a field type outside the
/// list is reported at the bound with the trait's diagnostic.
fn bounds(input: &DeriveInput, fields: &[Field<'_>]) -> (TokenStream, TokenStream, TokenStream) {
    let schema = schema();
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let mut predicates: Vec<TokenStream> =
        where_clause.map(|clause| clause.predicates.iter().map(|p| quote!(#p)).collect()).unwrap_or_default();
    predicates.extend(fields.iter().map(|field| {
        let ty = field.ty;
        quote!(#ty: #schema::Column)
    }));
    (quote!(#impl_generics), quote!(#ty_generics), quote!(where #(#predicates,)*))
}

/// The `Element` impl both shapes share: the row's columns are the fields' columns, unprefixed.
fn element_impl(input: &DeriveInput, fields: &[Field<'_>]) -> TokenStream {
    let schema = schema();
    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = bounds(input, fields);
    let tys: Vec<_> = fields.iter().map(|field| field.ty).collect();
    let names: Vec<_> = fields.iter().map(|field| &field.name).collect();
    let idents: Vec<_> = fields.iter().map(|field| field.ident).collect();
    let locals: Vec<_> = fields.iter().map(|field| &field.local).collect();
    quote! {
        #[automatically_derived]
        impl #impl_generics #schema::Element for #ident #ty_generics #where_clause {
            const TABLES: usize = 0 #(+ <#tys as #schema::Columns>::TABLES)*;

            fn layout_row(layout: &mut #schema::Layout) {
                #(<#tys as #schema::Columns>::layout(layout, #names, true);)*
            }

            fn encode_row(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                #(<#tys as #schema::Columns>::encode(&self.#idents, e).map_err(|error| error.at(#names))?;)*
                ::core::result::Result::Ok(())
            }

            fn decode_row(d: &mut #schema::Decoder) -> ::core::result::Result<Self, #schema::DecodeError> {
                #(
                    let #locals = <#tys as #schema::Columns>::decode(d)
                        .map_err(|error| error.at(#names))?
                        .ok_or_else(|| #schema::DecodeError::missing(#names))?;
                )*
                ::core::result::Result::Ok(Self { #(#idents: #locals),* })
            }
        }
    }
}

fn kind_impls(input: &DeriveInput, fields: &[Field<'_>], kind: &LitStr, code: u32) -> TokenStream {
    let schema = schema();
    let ident = &input.ident;
    let element = element_impl(input, fields);
    let (impl_generics, ty_generics, where_clause) = bounds(input, fields);
    quote! {
        #element

        #[automatically_derived]
        impl #impl_generics #schema::Kind for #ident #ty_generics #where_clause {
            const KIND: &'static str = #kind;
            const CODE: #schema::KindCode = #schema::KindCode(#code);

            fn table_set() -> #schema::TableSet {
                let mut layout = #schema::Layout::root(Self::KIND, Self::CODE);
                <Self as #schema::Element>::layout_row(&mut layout);
                layout.finish()
            }
        }
    }
}

fn part_impls(input: &DeriveInput, fields: &[Field<'_>]) -> TokenStream {
    let schema = schema();
    let ident = &input.ident;
    let element = element_impl(input, fields);
    let (impl_generics, ty_generics, where_clause) = bounds(input, fields);
    let tys: Vec<_> = fields.iter().map(|field| field.ty).collect();
    let names: Vec<_> = fields.iter().map(|field| &field.name).collect();
    let idents: Vec<_> = fields.iter().map(|field| field.ident).collect();
    let locals: Vec<_> = fields.iter().map(|field| &field.local).collect();
    quote! {
        #element

        #[automatically_derived]
        impl #impl_generics #schema::Columns for #ident #ty_generics #where_clause {
            const TABLES: usize = 0 #(+ <#tys as #schema::Columns>::TABLES)*;

            fn layout(layout: &mut #schema::Layout, name: &str, required: bool) {
                #(<#tys as #schema::Columns>::layout(layout, &#schema::Layout::join(name, #names), required);)*
            }

            fn encode(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                <Self as #schema::Element>::encode_row(self, e)
            }

            fn encode_absent(e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                #(<#tys as #schema::Columns>::encode_absent(e).map_err(|error| error.at(#names))?;)*
                ::core::result::Result::Ok(())
            }

            /// Absent when every field is absent; a partly present value is missing a column.
            fn decode(d: &mut #schema::Decoder) -> ::core::result::Result<::core::option::Option<Self>, #schema::DecodeError> {
                #(let #locals = <#tys as #schema::Columns>::decode(d).map_err(|error| error.at(#names))?;)*
                if #(#locals.is_none())&&* {
                    return ::core::result::Result::Ok(::core::option::Option::None);
                }
                ::core::result::Result::Ok(::core::option::Option::Some(Self {
                    #(#idents: #locals.ok_or_else(|| #schema::DecodeError::missing(#names))?,)*
                }))
            }

            fn decode_absent(d: &mut #schema::Decoder) -> ::core::result::Result<(), #schema::DecodeError> {
                #(<#tys as #schema::Columns>::decode_absent(d).map_err(|error| error.at(#names))?;)*
                ::core::result::Result::Ok(())
            }
        }

        #[automatically_derived]
        impl #impl_generics #schema::Flat for #ident #ty_generics #where_clause {}
    }
}

/// Refusal for struct forms with no storage shape.
pub(crate) fn refuse_empty(input: &DeriveInput) -> syn::Error {
    refuse(&input.ident, "a struct without fields has no columns to store")
}
