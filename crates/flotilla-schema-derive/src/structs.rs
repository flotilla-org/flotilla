//! Code generation for structs: kinds, parts and newtypes.
//!
//! A kind (`#[store(kind = "Name", code = N)]`) is the root row of its own tables and implements
//! `Element` and `Kind`. A part implements `Element` (it can be a collection's row) and, having no
//! table fields, `Columns` and `Flat` (it can be flattened into another row). A part's table fields
//! nest under its collection's table, and its declared indexes are on that table. A single-field
//! tuple struct is a transparent column.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    visit::{self, Visit},
    DeriveInput, FieldsNamed, FieldsUnnamed, LitStr, Type,
};

use crate::{
    attrs::{self, refuse, TypeAttrs},
    fields::{columns_element, schema, Field, Generics},
    index::Indexes,
    shape::{self, FieldShape},
};

pub(crate) fn expand(input: &DeriveInput, fields: &FieldsNamed, type_attrs: TypeAttrs) -> syn::Result<TokenStream> {
    refuse_labels(&type_attrs)?;
    let fields = fields.named.iter().map(|field| Field::parse(field, &input.ident, None)).collect::<syn::Result<Vec<_>>>()?;
    match (&type_attrs.kind, type_attrs.code) {
        (Some(kind), Some((code, _))) => {
            check_kind_name(kind)?;
            let indexes = Indexes::parse(&type_attrs.indexes, &fields)?;
            let handles = handles(input, &fields, true)?;
            let kind = kind_impls(input, &fields, kind, code, &indexes, type_attrs.edges.as_ref());
            Ok(quote!(#kind #handles))
        }
        (Some(kind), None) => Err(syn::Error::new_spanned(kind, "a kind needs a stable `code = N`")),
        (None, Some((_, span))) => Err(syn::Error::new(span, "`code` on a struct needs `kind = \"Name\"`")),
        (None, None) => {
            refuse_edges(&type_attrs)?;
            let indexes = Indexes::parse(&type_attrs.indexes, &fields)?;
            let handles = handles(input, &fields, false)?;
            let part = part_impls(input, &fields, &indexes);
            Ok(quote!(#part #handles))
        }
    }
}

/// Associated consts that name the stored traits' own items: an inherent const takes precedence
/// over a trait's in a path such as `Message::KIND`, so a handle with one of these names would
/// silently change what that path means. `TABLES` belongs to `Element` and `Columns` (every
/// struct), `KIND` and `CODE` to `Kind` (kinds only).
const TRAIT_ITEMS: &[&str] = &["TABLES"];
const KIND_ITEMS: &[&str] = &["KIND", "CODE"];

/// The struct's edge handles (see `flotilla_schema::Edge`): one associated const per field, named
/// by the field in upper case. A field whose handle would shadow a stored trait's item, or repeat
/// another field's handle, is refused.
fn handles(input: &DeriveInput, fields: &[Field<'_>], kind: bool) -> syn::Result<TokenStream> {
    let mut seen = std::collections::BTreeSet::new();
    for field in fields {
        let name = field.handle_name();
        if TRAIT_ITEMS.contains(&name.as_str()) || (kind && KIND_ITEMS.contains(&name.as_str())) {
            return Err(refuse(
                field.ident,
                format_args!(
                    "field `{}` would name its edge handle `{name}`, which the stored traits already use; rename the field",
                    field.name.value()
                ),
            ));
        }
        if !seen.insert(name.clone()) {
            return Err(refuse(field.ident, format_args!("two fields name the same edge handle `{name}`")));
        }
    }
    let ident = &input.ident;
    // The same bounds as `Element`, so a field outside the list is reported once, by its trait.
    let Generics { impl_generics, ty_generics, where_clause } = bounds(input, fields);
    let consts = fields.iter().filter_map(Field::handle);
    Ok(quote! {
        #[automatically_derived]
        #[allow(dead_code)]
        impl #impl_generics #ident #ty_generics #where_clause {
            #(#consts)*
        }
    })
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

/// `impl` header pieces with one bound per field, so a field type outside the list is reported at
/// the bound with the trait's diagnostic.
fn bounds<'a>(input: &'a DeriveInput, fields: &[Field<'_>]) -> Generics<'a> {
    Generics::new(&input.generics, fields.iter().map(Field::bound))
}

/// The `Element` impl both shapes share: the row's columns are the fields' columns, unprefixed.
/// `indexes` declares a part's indexes on the table holding its rows; a kind's go in `table_set`.
fn element_impl(input: &DeriveInput, fields: &[Field<'_>], indexes: TokenStream) -> TokenStream {
    let schema = schema();
    let ident = &input.ident;
    let Generics { impl_generics, ty_generics, where_clause } = bounds(input, fields);
    let tables = fields.iter().map(Field::tables);
    let layout = fields.iter().map(Field::layout_row_field);
    let encode = fields.iter().map(|field| {
        let ident = field.ident;
        field.encode(&quote!(&self.#ident))
    });
    let decode = fields.iter().map(Field::decode_required);
    let idents: Vec<_> = fields.iter().map(|field| field.ident).collect();
    let locals: Vec<_> = fields.iter().map(|field| &field.local).collect();
    quote! {
        #[automatically_derived]
        impl #impl_generics #schema::Element for #ident #ty_generics #where_clause {
            const TABLES: usize = 0 #(+ #tables)*;

            fn layout_row(layout: &mut #schema::Layout) {
                #(#layout)*
                #indexes
            }

            fn encode_row(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                #(#encode)*
                ::core::result::Result::Ok(())
            }

            fn decode_row(d: &mut #schema::Decoder) -> ::core::result::Result<Self, #schema::DecodeError> {
                #(#decode)*
                ::core::result::Result::Ok(Self { #(#idents: #locals),* })
            }
        }
    }
}

/// The `Kind` impl's bounds are only those of fields that mention a type parameter: a concrete
/// field such as `in_reply_to: Option<Ref<Message>>` needs `Message: Kind`, and proving it must not
/// lead back through `Message`'s own field bounds. Concrete fields are still checked through the
/// `Element` supertrait. Declared indexes join the descriptor after the root's layout, and each
/// gets a query constructor.
fn kind_impls(
    input: &DeriveInput,
    fields: &[Field<'_>],
    kind: &LitStr,
    code: u32,
    indexes: &Indexes<'_, '_>,
    edges: Option<&syn::Path>,
) -> TokenStream {
    let schema = schema();
    let ident = &input.ident;
    let element = element_impl(input, fields, TokenStream::new());
    let params: Vec<_> = input.generics.type_params().map(|param| param.ident.to_string()).collect();
    let generic = fields.iter().filter(|field| mentions(field.ty, &params)).map(Field::bound);
    let Generics { impl_generics, ty_generics, where_clause } = Generics::new(&input.generics, generic);
    let index_layout = indexes.layout();
    let constructors = indexes.constructors(input);
    let edges = edges.map(|edges| quote!(layout.edges(<#edges as #schema::CodeEnum>::CODES);));
    quote! {
        #element

        #[automatically_derived]
        impl #impl_generics #schema::Kind for #ident #ty_generics #where_clause {
            const KIND: &'static str = #kind;
            const CODE: #schema::KindCode = #schema::KindCode(#code);

            fn table_set() -> #schema::TableSet {
                let mut layout = #schema::Layout::root(<Self as #schema::Kind>::KIND, <Self as #schema::Kind>::CODE);
                <Self as #schema::Element>::layout_row(&mut layout);
                #index_layout
                #edges
                layout.finish()
            }
        }

        #constructors
    }
}

/// Whether `ty` names any of the type parameters.
fn mentions(ty: &Type, params: &[String]) -> bool {
    struct Finder<'p> {
        params: &'p [String],
        found: bool,
    }
    impl<'ast> Visit<'ast> for Finder<'_> {
        fn visit_ident(&mut self, ident: &'ast syn::Ident) {
            self.found |= self.params.iter().any(|param| ident == param);
        }
    }
    let mut finder = Finder { params, found: false };
    visit::visit_type(&mut finder, ty);
    finder.found
}

/// A part is a collection's row (`Element`). Without table fields or indexes it is also `Columns`
/// and `Flat`, so it can be flattened into another row; a part holding tables or declaring indexes
/// is only ever a row, so the missing `Flat` reports `flatten` of it at the bound.
fn part_impls(input: &DeriveInput, fields: &[Field<'_>], indexes: &Indexes<'_, '_>) -> TokenStream {
    let schema = schema();
    let ident = &input.ident;
    let element = element_impl(input, fields, indexes.layout());
    if fields.iter().any(Field::is_table) || !indexes.is_empty() {
        return element;
    }
    let Generics { impl_generics, ty_generics, where_clause } = bounds(input, fields);
    let tables = fields.iter().map(Field::tables);
    let layout = fields.iter().map(|field| field.layout(&quote!(name), &quote!(required)));
    let encode_absent = fields.iter().map(Field::encode_absent);
    let decode = fields.iter().map(Field::decode_optional);
    let decode_absent = fields.iter().map(Field::decode_absent);
    let idents: Vec<_> = fields.iter().map(|field| field.ident).collect();
    let locals: Vec<_> = fields.iter().map(|field| &field.local).collect();
    let names: Vec<_> = fields.iter().map(|field| &field.name).collect();
    quote! {
        #element

        #[automatically_derived]
        impl #impl_generics #schema::Columns for #ident #ty_generics #where_clause {
            const TABLES: usize = 0 #(+ #tables)*;

            fn layout(layout: &mut #schema::Layout, name: &str, required: bool) {
                #(#layout)*
            }

            fn encode(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                <Self as #schema::Element>::encode_row(self, e)
            }

            fn encode_absent(e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                #(#encode_absent)*
                ::core::result::Result::Ok(())
            }

            /// Absent when every field is absent; a partly present value is missing a column.
            fn decode(d: &mut #schema::Decoder) -> ::core::result::Result<::core::option::Option<Self>, #schema::DecodeError> {
                #(#decode)*
                if #(#locals.is_none())&&* {
                    return ::core::result::Result::Ok(::core::option::Option::None);
                }
                ::core::result::Result::Ok(::core::option::Option::Some(Self {
                    #(#idents: #locals.ok_or_else(|| #schema::DecodeError::missing(#names))?,)*
                }))
            }

            fn decode_absent(d: &mut #schema::Decoder) -> ::core::result::Result<(), #schema::DecodeError> {
                #(#decode_absent)*
                ::core::result::Result::Ok(())
            }
        }

        #[automatically_derived]
        impl #impl_generics #schema::Flat for #ident #ty_generics #where_clause {}
    }
}

/// A single-field tuple struct is transparent: its columns are its field's, under the same name.
/// It is a `Column` (and an `Element`), and `Intern` when it wraps a `String`.
pub(crate) fn newtype(input: &DeriveInput, fields: &FieldsUnnamed, type_attrs: TypeAttrs) -> syn::Result<TokenStream> {
    refuse_labels(&type_attrs)?;
    refuse_edges(&type_attrs)?;
    let field = &fields.unnamed[0];
    let field_attrs = attrs::field_attrs(&field.attrs)?;
    if let Some(attr) = field_attrs.shape {
        return Err(refuse(
            &field.ty,
            format_args!("a newtype's field takes no storage attribute; put `{}` on the field that holds the newtype", attr.name()),
        ));
    }
    if let Some(alias) = field_attrs.alias {
        return Err(refuse(alias, "a newtype's field has no name of its own; put `alias` on the field that holds the newtype"));
    }
    if shape::classify(None, &field.ty, &input.ident)? != FieldShape::Column {
        return Err(refuse(&field.ty, "a newtype wraps a column type"));
    }
    if let Some(kind) = &type_attrs.kind {
        return Err(refuse(kind, "a kind is a struct with named fields"));
    }
    if let Some(index) = type_attrs.indexes.first() {
        return Err(refuse(index, "`index` belongs on a kind or a part struct"));
    }
    if let Some((_, span)) = type_attrs.code {
        return Err(syn::Error::new(span, "`code` on a struct needs `kind = \"Name\"`"));
    }
    let schema = schema();
    let ident = &input.ident;
    let ty = &field.ty;
    let generics = Generics::new(&input.generics, [quote!(#ty: #schema::Column)]);
    let Generics { impl_generics, ty_generics, where_clause } = &generics;
    let element = columns_element(ident, &generics);
    let key_where = Generics::new(&input.generics, [quote!(for<'__store> #ty: #schema::KeyColumn)]).where_clause;
    let wraps_string = matches!(ty, Type::Path(path) if path.qself.is_none() && path.path.is_ident("String"));
    let intern = wraps_string.then(|| {
        let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
        quote! {
            #[automatically_derived]
            impl #impl_generics #schema::Intern for #ident #ty_generics #where_clause {
                fn as_str(&self) -> &str {
                    &self.0
                }

                fn from_string(value: ::std::string::String) -> Self {
                    Self(value)
                }
            }
        }
    });
    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics #schema::Columns for #ident #ty_generics #where_clause {
            const TABLES: usize = <#ty as #schema::Columns>::TABLES;

            fn layout(layout: &mut #schema::Layout, name: &str, required: bool) {
                <#ty as #schema::Columns>::layout(layout, name, required);
            }

            fn encode(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                <#ty as #schema::Columns>::encode(&self.0, e)
            }

            fn encode_absent(e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                <#ty as #schema::Columns>::encode_absent(e)
            }

            fn decode(d: &mut #schema::Decoder) -> ::core::result::Result<::core::option::Option<Self>, #schema::DecodeError> {
                ::core::result::Result::Ok(<#ty as #schema::Columns>::decode(d)?.map(Self))
            }

            fn decode_absent(d: &mut #schema::Decoder) -> ::core::result::Result<(), #schema::DecodeError> {
                <#ty as #schema::Columns>::decode_absent(d)
            }
        }

        #[automatically_derived]
        impl #impl_generics #schema::Column for #ident #ty_generics #where_clause {}

        // The higher-ranked bound defers the check to each use as a key, so a newtype of a type
        // that cannot key a table (an option, a data enum) still compiles as a column.
        #[automatically_derived]
        impl #impl_generics #schema::KeyColumn for #ident #ty_generics #key_where {}

        #element

        #intern
    })
}

/// `labels` names a machine's label enum, so it belongs on the state enum, not a struct.
fn refuse_edges(type_attrs: &TypeAttrs) -> syn::Result<()> {
    match &type_attrs.edges {
        Some(edges) => Err(refuse(edges, "`edges` belongs on a kind, whose log entries it constrains")),
        None => Ok(()),
    }
}

fn refuse_labels(type_attrs: &TypeAttrs) -> syn::Result<()> {
    match &type_attrs.labels {
        Some(labels) => Err(refuse(labels, "`labels` belongs on an enum whose variants are a machine's labels, not a struct")),
        None => Ok(()),
    }
}

/// Refusal for struct forms with no storage shape.
pub(crate) fn refuse_empty(input: &DeriveInput) -> syn::Error {
    refuse(&input.ident, "a struct without fields has no columns to store")
}
