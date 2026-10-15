//! One stored field (of a struct or of a data enum variant) and the code generated for it.
//!
//! Every field goes through the `::flotilla_schema::adapt` adapter its attribute selects, and
//! adds one where-clause bound that names the trait its shape needs (`Column`, `Intern`, `Flat`
//! or `CodeEnum`), so an unsupported field type is reported with that trait's diagnostic.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{ext::IdentExt, Field as SynField, Ident, LitStr, Type};

use crate::{
    attrs,
    shape::{self, Collection, FieldShape},
};

pub(crate) struct Field<'a> {
    pub ident: &'a Ident,
    /// A local for the field's value that cannot shadow the generated code's own bindings.
    pub local: Ident,
    /// The column name below the enclosing prefix: the Rust field name (never a serde name),
    /// preceded by the snake_case variant name inside a data enum.
    pub name: LitStr,
    pub ty: &'a Type,
    pub shape: FieldShape,
}

impl<'a> Field<'a> {
    /// Classifies a named field; `variant` is the snake_case variant name inside a data enum.
    pub(crate) fn parse(field: &'a SynField, owner: &Ident, variant: Option<&str>) -> syn::Result<Field<'a>> {
        let ident = field.ident.as_ref().expect("named field");
        let shape = shape::classify(attrs::field_attr(&field.attrs)?, &field.ty, owner)?;
        let plain = ident.unraw().to_string();
        let name = match variant {
            Some(variant) => format!("{variant}_{plain}"),
            None => plain.clone(),
        };
        let local = match variant {
            Some(variant) => format_ident!("__store_{}_{}", variant, plain),
            None => format_ident!("__store_{}", plain),
        };
        Ok(Field { ident, local, name: LitStr::new(&name, ident.span()), ty: &field.ty, shape })
    }

    /// The adapter type implementing `Adapter<FieldTy>` for this field's attribute.
    fn adapter(&self) -> TokenStream {
        let schema = schema();
        match self.shape {
            FieldShape::Intern { optional: false } => quote!(#schema::adapt::AsInterned),
            FieldShape::Intern { optional: true } => quote!(#schema::adapt::AsOptionalInterned),
            FieldShape::Flatten { optional: true } => quote!(#schema::adapt::AsOptionalFlat),
            FieldShape::Frozen { optional: false } => quote!(#schema::adapt::AsFrozen),
            FieldShape::Frozen { optional: true } => quote!(#schema::adapt::AsOptionalFrozen),
            FieldShape::Child(_) => quote!(#schema::adapt::AsChild),
            FieldShape::Records => quote!(#schema::adapt::AsRecords),
            FieldShape::History => quote!(#schema::adapt::AsHistory),
            FieldShape::Column | FieldShape::Flatten { optional: false } | FieldShape::State => quote!(#schema::adapt::AsColumns),
        }
    }

    /// `<Adapter as Adapter<FieldTy>>`, the path every generated call goes through.
    fn via(&self) -> TokenStream {
        let schema = schema();
        let (adapter, ty) = (self.adapter(), self.ty);
        quote!(<#adapter as #schema::adapt::Adapter<#ty>>)
    }

    /// The where-clause bound naming the trait this field's shape needs.
    pub(crate) fn bound(&self) -> TokenStream {
        let schema = schema();
        let ty = self.ty;
        let inner = || shape::first_arg(ty).unwrap_or(ty);
        match self.shape {
            FieldShape::Intern { optional: false } => quote!(#ty: #schema::Intern),
            FieldShape::Intern { optional: true } => {
                let inner = inner();
                quote!(#inner: #schema::Intern)
            }
            FieldShape::Flatten { optional: false } => quote!(#ty: #schema::Flat),
            FieldShape::Flatten { optional: true } => {
                let inner = inner();
                quote!(#inner: #schema::Flat)
            }
            FieldShape::State => {
                let inner = inner();
                quote!(#inner: #schema::CodeEnum)
            }
            FieldShape::Frozen { optional: false } => {
                let inner = inner();
                quote!(#inner: #schema::Freezable)
            }
            FieldShape::Frozen { optional: true } => {
                let inner = shape::first_arg(inner()).unwrap_or(ty);
                quote!(#inner: #schema::Freezable)
            }
            FieldShape::Child(Collection::Vec) | FieldShape::Records | FieldShape::History => {
                let inner = inner();
                quote!(#inner: #schema::Element)
            }
            FieldShape::Child(Collection::Set) => {
                let inner = inner();
                quote!(#inner: #schema::KeyColumn + ::core::cmp::Ord)
            }
            FieldShape::Child(Collection::Map) => {
                let (key, value) = shape::map_args(ty).unwrap_or((ty, ty));
                quote!(#key: #schema::KeyColumn + ::core::cmp::Ord, #value: #schema::Element)
            }
            FieldShape::Column => quote!(#ty: #schema::Column),
        }
    }

    /// Whether the field is stored in a table of its own rather than in columns of the row.
    pub(crate) fn is_table(&self) -> bool {
        matches!(self.shape, FieldShape::Child(_) | FieldShape::Records | FieldShape::History)
    }

    pub(crate) fn tables(&self) -> TokenStream {
        let via = self.via();
        quote!(#via::TABLES)
    }

    /// Declares the field's columns under `prefix` (an expression of type `&str`).
    pub(crate) fn layout(&self, prefix: &TokenStream, required: &TokenStream) -> TokenStream {
        let (schema, via, name) = (schema(), self.via(), &self.name);
        quote!(#via::layout(layout, &#schema::Layout::join(#prefix, #name), #required);)
    }

    /// Encodes `value` (an expression of type `&FieldTy`).
    pub(crate) fn encode(&self, value: &TokenStream) -> TokenStream {
        let (via, name) = (self.via(), &self.name);
        quote!(#via::encode(#value, e).map_err(|error| error.at(#name))?;)
    }

    pub(crate) fn encode_absent(&self) -> TokenStream {
        let (via, name) = (self.via(), &self.name);
        quote!(#via::encode_absent(e).map_err(|error| error.at(#name))?;)
    }

    /// Binds the field's local to its decoded value: `Option<FieldTy>`, `None` when absent.
    pub(crate) fn decode_optional(&self) -> TokenStream {
        let (via, name, local) = (self.via(), &self.name, &self.local);
        quote!(let #local = #via::decode(d).map_err(|error| error.at(#name))?;)
    }

    /// Binds the field's local to its decoded value, refusing an absent one.
    pub(crate) fn decode_required(&self) -> TokenStream {
        let (schema, via, name, local) = (schema(), self.via(), &self.name, &self.local);
        quote! {
            let #local = #via::decode(d).map_err(|error| error.at(#name))?.ok_or_else(|| #schema::DecodeError::missing(#name))?;
        }
    }

    pub(crate) fn decode_absent(&self) -> TokenStream {
        let (via, name) = (self.via(), &self.name);
        quote!(#via::decode_absent(d).map_err(|error| error.at(#name))?;)
    }
}

pub(crate) fn schema() -> TokenStream {
    quote!(::flotilla_schema)
}

/// The `Element` impl of a type whose row is its `Columns` with an empty prefix: enums and newtypes.
/// A scalar's column is then named `value`, a data enum's tag `tag`.
pub(crate) fn columns_element(ident: &Ident, generics: &Generics<'_>) -> TokenStream {
    let schema = schema();
    let Generics { impl_generics, ty_generics, where_clause } = generics;
    quote! {
        #[automatically_derived]
        impl #impl_generics #schema::Element for #ident #ty_generics #where_clause {
            const TABLES: usize = <Self as #schema::Columns>::TABLES;

            fn layout_row(layout: &mut #schema::Layout) {
                <Self as #schema::Columns>::layout(layout, "", true);
            }

            fn encode_row(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                <Self as #schema::Columns>::encode(self, e)
            }

            fn decode_row(d: &mut #schema::Decoder) -> ::core::result::Result<Self, #schema::DecodeError> {
                <Self as #schema::Columns>::decode(d)?.ok_or_else(|| #schema::DecodeError::missing(""))
            }
        }
    }
}

/// `impl` header pieces: the type's generics plus extra where-clause predicates.
pub(crate) struct Generics<'a> {
    pub impl_generics: syn::ImplGenerics<'a>,
    pub ty_generics: syn::TypeGenerics<'a>,
    pub where_clause: TokenStream,
}

impl<'a> Generics<'a> {
    pub(crate) fn new(generics: &'a syn::Generics, extra: impl IntoIterator<Item = TokenStream>) -> Generics<'a> {
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        let mut predicates: Vec<TokenStream> =
            where_clause.map(|clause| clause.predicates.iter().map(|p| quote!(#p)).collect()).unwrap_or_default();
        predicates.extend(extra);
        Generics { impl_generics, ty_generics, where_clause: quote!(where #(#predicates,)*) }
    }
}
