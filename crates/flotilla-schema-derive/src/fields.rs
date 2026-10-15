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
    pub vis: &'a syn::Visibility,
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
        Ok(Field { ident, local, name: LitStr::new(&name, ident.span()), ty: &field.ty, vis: &field.vis, shape })
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
            FieldShape::Cbor => quote!(#schema::adapt::AsCbor),
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
            FieldShape::Cbor => quote!(#ty: #schema::Cbor),
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

    /// Declares the field as one of its row's own fields: column-shaped fields are recorded in the
    /// table's `fields`, the unit an edge sets; table fields declare their table.
    pub(crate) fn layout_row_field(&self) -> TokenStream {
        let layout = self.layout(&quote!(""), &quote!(true));
        if self.is_table() {
            return layout;
        }
        let (name, state) = (&self.name, self.shape == FieldShape::State);
        quote!(layout.field(#name, #state, |layout| { #layout });)
    }

    /// The edge handle's name: the field name in upper case (`state` is `STATE`).
    pub(crate) fn handle_name(&self) -> String {
        self.ident.unraw().to_string().to_uppercase()
    }

    /// The field's edge handle, an associated const of the struct: a `StateOf` for a state field,
    /// a `ColumnOf` for another column-shaped field, a `RecordsOf` or `HistoryOf` for a records or
    /// history field, and none for a spec collection, which changes only by a spec edit.
    pub(crate) fn handle(&self) -> Option<TokenStream> {
        let (schema, via, name, ty, vis) = (schema(), self.via(), &self.name, self.ty, self.vis);
        let handle = format_ident!("{}", self.handle_name(), span = self.ident.span());
        let inner = || shape::first_arg(ty).unwrap_or(ty);
        let (doc, ty, value) = match self.shape {
            FieldShape::Child(_) => return None,
            FieldShape::Records => {
                let inner = inner();
                ("records", quote!(#schema::RecordsOf<Self, #inner>), quote!(#schema::RecordsOf::new(#name)))
            }
            FieldShape::History => {
                let inner = inner();
                ("history", quote!(#schema::HistoryOf<Self, #inner>), quote!(#schema::HistoryOf::new(#name)))
            }
            FieldShape::State => (
                "state",
                quote!(#schema::StateOf<Self, #ty>),
                quote!(#schema::StateOf::new(#schema::ColumnOf::new(#name, #via::TABLES, #via::encode))),
            ),
            _ => ("column", quote!(#schema::ColumnOf<Self, #ty>), quote!(#schema::ColumnOf::new(#name, #via::TABLES, #via::encode))),
        };
        let doc = format!("Edge handle of the {doc} field `{}`.", name.value());
        Some(quote! {
            #[doc = #doc]
            #vis const #handle: #ty = #value;
        })
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
