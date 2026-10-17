//! Code generation for enums: unit code enums, data enums and machine states.
//!
//! Every variant carries an explicit `#[store(code = "..")]`: short text written by hand (#3392
//! ruling G7); serde names and declaration order never reach storage, and the derive refuses a
//! code used twice. An enum whose variants are all units is a code enum: one `TEXT` column with
//! `CHECK (c IN (codes))`, implementing `CodeEnum`.
//!
//! An enum with a named-field variant is a data enum, stored by mapping B (#3392 ruling G6): its
//! tag column, then each datum once, named `<tag>_<datum>` and shared by name across the variants
//! that carry it, then one CHECK per variant requiring the data it carries and nulling the data it
//! lacks. An enum with `#[store(labels = L)]` is a machine's state (#3373 ruling 1), stored the
//! same way under `#[store(state)]` as `State<Self>`: its variants are the labels of the code enum
//! `L`, code for code, its label column is the state field, and its datums are named by the datum
//! alone.
//!
//! A datum has one type and attribute in every variant carrying it (#3392 ruling D3: interned data
//! of different Rust types does not share a column either), except that a datum may be `T` in one
//! variant and `Option<T>` in another (#3392 ruling G5): one nullable column group, required by the
//! CHECKs of the variants carrying `T`. A datum's table holds rows only while the variant
//! carries it, and decoding refuses rows otherwise. A state's datum is unattributed (its columns),
//! `child` (a `BTreeMap` or `BTreeSet`, changed row by row), `intern` or `flatten`; a data enum's
//! datum may take any field form.

use std::collections::BTreeMap;

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{ext::IdentExt, DataEnum, DeriveInput, Fields, Ident};

use crate::{
    attrs::{self, refuse, refuse_at, TypeAttrs},
    fields::{columns_element, schema, Field, Generics},
    shape::{self, Collection, FieldShape},
};

struct Variant<'a> {
    ident: &'a Ident,
    code: String,
    /// `None` for a unit variant.
    fields: Option<Vec<Field<'a>>>,
}

pub(crate) fn expand(input: &DeriveInput, data: &DataEnum, type_attrs: TypeAttrs) -> syn::Result<TokenStream> {
    if let Some(kind) = &type_attrs.kind {
        return Err(refuse(kind, "a kind is a struct with named fields, not an enum"));
    }
    if let Some((_, span)) = type_attrs.code {
        return Err(refuse_at(span, "an enum takes `code = \"..\"` on each variant, not on the type"));
    }
    if let Some(index) = type_attrs.indexes.first() {
        return Err(refuse(index, "`index` belongs on a kind or a part struct"));
    }
    if let Some(state) = &type_attrs.state {
        return Err(refuse(state, "`state = ..` belongs on a kind's spec struct"));
    }
    if let Some(edges) = &type_attrs.edges {
        return Err(refuse(edges, "`edges` belongs on a kind, whose log entries it constrains"));
    }
    if data.variants.is_empty() {
        return Err(refuse(&input.ident, "an enum without variants has no values to store"));
    }
    let labels = type_attrs.labels.as_ref();
    let mut seen: BTreeMap<String, &Ident> = BTreeMap::new();
    let mut variants = Vec::new();
    for variant in &data.variants {
        let code = attrs::variant_code(&variant.attrs)?;
        let fields = match &variant.fields {
            Fields::Unnamed(fields) => return Err(refuse(fields, "tuple variants cannot be stored; use named fields")),
            Fields::Unit => None,
            // A datum is named by the field alone: shared by name across the variants.
            Fields::Named(fields) => Some(
                fields
                    .named
                    .iter()
                    .map(|field| Field::parse(field, &input.ident, None).map(|field| Field { state_datum: labels.is_some(), ..field }))
                    .collect::<syn::Result<Vec<_>>>()?,
            ),
        };
        let Some((code, span)) = code else {
            return Err(refuse(&variant.ident, format_args!("variant `{}` needs a stable `#[store(code = \"..\")]`", variant.ident)));
        };
        if let Some(previous) = seen.insert(code.clone(), &variant.ident) {
            return Err(refuse_at(span, format_args!("code {code:?} is used by both `{previous}` and `{}`", variant.ident)));
        }
        variants.push(Variant { ident: &variant.ident, code, fields });
    }
    // Ascending by code, each with the Rust name of the variant carrying it.
    let codes: Vec<(String, String)> = seen.into_iter().map(|(code, variant)| (code, variant.unraw().to_string())).collect();
    if let Some(labels) = labels {
        return labels_enum(input, labels, &variants, &codes);
    }
    if variants.iter().all(|variant| variant.fields.is_none()) {
        Ok(code_enum(input, &variants, &codes))
    } else {
        data_enum(input, &variants, &codes)
    }
}

/// `const { Code::new("..") }`: the code, checked again by the schema crate when it compiles.
fn code_expr(code: &str) -> TokenStream {
    let schema = schema();
    quote!(const { #schema::Code::new(#code) })
}

/// `&[Code::new(..), ..]` of `codes`.
fn codes_expr(codes: &[(String, String)]) -> TokenStream {
    let codes = codes.iter().map(|(code, _)| code_expr(code));
    quote!(&[#(#codes),*])
}

/// `&["Name", ..]`: the variant names of `codes`, in the same order.
fn names_expr(codes: &[(String, String)]) -> TokenStream {
    let names = codes.iter().map(|(_, name)| name);
    quote!(&[#(#names),*])
}

/// Arms mapping each variant to its code.
fn code_arms(variants: &[Variant<'_>]) -> Vec<TokenStream> {
    variants
        .iter()
        .map(|variant| {
            let (ident, code) = (variant.ident, code_expr(&variant.code));
            match variant.fields {
                None => quote!(Self::#ident => #code),
                Some(_) => quote!(Self::#ident { .. } => #code),
            }
        })
        .collect()
}

fn code_enum(input: &DeriveInput, variants: &[Variant<'_>], codes: &[(String, String)]) -> TokenStream {
    let schema = schema();
    let ident = &input.ident;
    let generics = Generics::new(&input.generics, []);
    let Generics { impl_generics, ty_generics, where_clause } = &generics;
    let element = columns_element(ident, &generics);
    let arms = code_arms(variants);
    let all = codes_expr(codes);
    let names = names_expr(codes);
    let decode_arms = variants.iter().map(|variant| {
        let (ident, code) = (variant.ident, &variant.code);
        quote!(#code => ::core::result::Result::Ok(Self::#ident))
    });
    quote! {
        #[automatically_derived]
        impl #impl_generics #schema::Columns for #ident #ty_generics #where_clause {
            const TABLES: usize = 0;

            fn layout(layout: &mut #schema::Layout, name: &str, required: bool) {
                layout.code(name, <Self as #schema::CodeEnum>::CODES, <Self as #schema::CodeEnum>::NAMES, required);
            }

            fn encode(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                e.put(#schema::code_value(<Self as #schema::CodeEnum>::code(self)));
                ::core::result::Result::Ok(())
            }

            fn encode_absent(e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                e.put(#schema::Value::Null);
                ::core::result::Result::Ok(())
            }

            fn decode(d: &mut #schema::Decoder) -> ::core::result::Result<::core::option::Option<Self>, #schema::DecodeError> {
                match #schema::decode_code(d, <Self as #schema::CodeEnum>::CODES)? {
                    ::core::option::Option::Some(code) => <Self as #schema::StateData>::decode_data(code, d).map(::core::option::Option::Some),
                    ::core::option::Option::None => ::core::result::Result::Ok(::core::option::Option::None),
                }
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
        impl #impl_generics #schema::KeyColumn for #ident #ty_generics #where_clause {}

        #[automatically_derived]
        impl #impl_generics #schema::CodeEnum for #ident #ty_generics #where_clause {
            const CODES: &'static [#schema::Code] = #all;
            const NAMES: &'static [&'static str] = #names;

            fn code(&self) -> #schema::Code {
                match self {
                    #(#arms,)*
                }
            }
        }

        // A code enum is its own label: a state that carries no data.
        #[automatically_derived]
        impl #impl_generics #schema::Labelled for #ident #ty_generics #where_clause {
            type Label = Self;

            fn label(&self) -> &Self {
                self
            }
        }

        #[automatically_derived]
        impl #impl_generics #schema::StateData for #ident #ty_generics #where_clause {
            const TABLES: usize = 0;
            const CODES: &'static [#schema::Code] = #all;
            const NAMES: &'static [&'static str] = #names;
            const DATA: &'static [&'static str] = &[];

            fn code(&self) -> #schema::Code {
                <Self as #schema::CodeEnum>::code(self)
            }

            fn layout_data(_layout: &mut #schema::Layout, _label: &str) {}

            fn encode_data(&self, _e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                ::core::result::Result::Ok(())
            }

            fn encode_absent_data(_e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                ::core::result::Result::Ok(())
            }

            fn decode_data(code: #schema::Code, _d: &mut #schema::Decoder) -> ::core::result::Result<Self, #schema::DecodeError> {
                match code.as_str() {
                    #(#decode_arms,)*
                    _ => ::core::result::Result::Err(#schema::DecodeError::Code { column: ::std::string::String::new(), code: code.to_string() }),
                }
            }

            fn decode_absent_data(_d: &mut #schema::Decoder) -> ::core::result::Result<(), #schema::DecodeError> {
                ::core::result::Result::Ok(())
            }

            fn delta(_previous: &Self, _next: &Self, _delta: &mut #schema::StateDelta) -> ::core::result::Result<(), #schema::EncodeError> {
                ::core::result::Result::Ok(())
            }
        }

        #element
    }
}

/// One variant's declaration of a datum.
struct Decl<'v, 'a> {
    variant: &'a Ident,
    code: String,
    field: &'v Field<'a>,
    /// `Option<T>` where another variant may declare `T`.
    optional: bool,
}

/// One datum of a data enum or a state: its declarations, by the variants carrying it.
struct Datum<'v, 'a> {
    name: String,
    decls: Vec<Decl<'v, 'a>>,
    /// Its index among the enum's datums.
    index: usize,
}

impl<'v, 'a> Datum<'v, 'a> {
    /// The declaration that lays the datum out: an optional one when any variant declares
    /// `Option<T>`, so the columns are nullable, else the first.
    fn layout_decl(&self) -> &Decl<'v, 'a> {
        self.decls.iter().find(|decl| decl.optional).unwrap_or(&self.decls[0])
    }

    /// The declaration as `T`, when the datum is `T` under some variants and `Option<T>` under
    /// others: its required columns are required under the variants carrying `T`.
    fn required_decl(&self) -> Option<&Decl<'v, 'a>> {
        let mixed = self.decls.iter().any(|decl| decl.optional) && self.decls.iter().any(|decl| !decl.optional);
        mixed.then(|| self.decls.iter().find(|decl| !decl.optional)).flatten()
    }

    fn field(&self) -> &'v Field<'a> {
        self.layout_decl().field
    }

    fn is_table(&self) -> bool {
        self.field().is_table()
    }

    /// The local holding the decoded datum of the `index`th declaration.
    fn local(&self, index: usize) -> Ident {
        format_ident!("{}_{}", self.decls[0].field.local, index)
    }

    /// The index of `variant`'s declaration of this datum.
    fn decl_of(&self, variant: &Ident) -> Option<usize> {
        self.decls.iter().position(|decl| decl.variant == variant)
    }

    /// Writes the datum's columns of `value` (an expression of type `&Self`): each declaration
    /// through its own adapter, and absent under a variant lacking it.
    fn encode(&self, value: &TokenStream) -> TokenStream {
        let arms = self.decls.iter().map(|decl| {
            let (variant, ident, local) = (decl.variant, decl.field.ident, &decl.field.local);
            let encode = decl.field.encode(&quote!(#local));
            quote!(Self::#variant { #ident: #local, .. } => { #encode })
        });
        let absent = self.field().encode_absent();
        quote! {
            match #value {
                #(#arms)*
                #[allow(unreachable_patterns)]
                _ => { #absent }
            }
        }
    }

    /// Binds the datum's locals for the variant whose code is `code` (a `Code`): each
    /// declaration's local is `Some` under its variant, and the datum's columns are refused when
    /// present under a variant lacking it.
    fn decode(&self, code: &TokenStream) -> TokenStream {
        let schema = schema();
        let name = &self.decls[0].field.name;
        let declare = (0..self.decls.len()).map(|index| {
            let local = self.local(index);
            quote!(let mut #local = ::core::option::Option::None;)
        });
        let branches = self.decls.iter().enumerate().map(|(index, decl)| {
            let (local, code_text, via) = (self.local(index), &decl.code, decl.field.via());
            quote! {
                if #code.as_str() == #code_text {
                    #local = ::core::option::Option::Some(
                        #via::decode(d).map_err(|error| error.at(#name))?.ok_or_else(|| #schema::DecodeError::missing(#name))?,
                    );
                } else
            }
        });
        let absent = self.field().decode_absent();
        quote! {
            #(#declare)*
            #(#branches)* {
                #absent
            }
        }
    }

    /// Declares the datum's columns (prefixed by `prefix`) through `scope`, which wraps a layout
    /// closure in `layout.scope(..)` or `layout.datum(..)`, giving its `DatumColumns`.
    fn layout(&self, prefix: &TokenStream, scope: impl Fn(TokenStream) -> TokenStream) -> TokenStream {
        let schema = schema();
        let layout = self.field().layout(prefix, &quote!(true));
        let declare = scope(quote!(|layout: &mut #schema::Layout| { #layout }));
        let required = self.required_decl().map(|decl| {
            let layout = decl.field.layout(prefix, &quote!(true));
            quote!(columns.required = #schema::Layout::required_of(|layout: &mut #schema::Layout| { #layout });)
        });
        quote! {{
            #[allow(unused_mut)]
            let mut columns = #declare;
            #required
            columns
        }}
    }
}

/// The type and attribute of a declaration, without an `Option` a variant may add (G5): `Some`
/// with the inner type when the declaration is optional in that sense.
fn declared(field: &Field<'_>) -> (String, FieldShape, bool) {
    let ty = field.ty;
    let text = |ty: &syn::Type| quote!(#ty).to_string();
    let optional_shape = match field.shape {
        FieldShape::Column => Some(FieldShape::Column),
        FieldShape::Intern { .. } => Some(FieldShape::Intern { optional: false }),
        FieldShape::Flatten { .. } => Some(FieldShape::Flatten { optional: false }),
        FieldShape::Frozen { .. } => Some(FieldShape::Frozen { optional: false }),
        _ => None,
    };
    match (optional_shape, shape::option_inner(ty)) {
        (Some(base), Some(inner)) => (text(inner), base, true),
        (Some(base), None) => (text(ty), base, false),
        (None, _) => (text(ty), field.shape, false),
    }
}

/// The datums of `variants`, in order of first declaration, refusing one declared with different
/// types or attributes (other than `T` against `Option<T>`), and, for a state, a datum outside the
/// storage forms a state datum takes.
fn datums<'v, 'a>(variants: &'v [Variant<'a>], state: bool) -> syn::Result<Vec<Datum<'v, 'a>>> {
    let mut datums: Vec<Datum<'v, 'a>> = Vec::new();
    for variant in variants {
        for field in variant.fields.iter().flatten() {
            let name = field.name.value();
            if state
                && !matches!(
                    field.shape,
                    FieldShape::Column
                        | FieldShape::Intern { .. }
                        | FieldShape::Flatten { .. }
                        | FieldShape::Child(Collection::Map | Collection::Set)
                )
            {
                return Err(refuse(
                    field.ident,
                    format_args!(
                        "state datum `{name}` takes no attribute (its columns), `child` (a BTreeMap or BTreeSet, changed row by row), `intern` or `flatten` (a struct without table fields, or an Option of one)"
                    ),
                ));
            }
            let (ty, shape, optional) = declared(field);
            let decl = Decl { variant: variant.ident, code: variant.code.clone(), field, optional };
            match datums.iter_mut().find(|datum| datum.name == name) {
                Some(datum) => {
                    let first = &datum.decls[0];
                    let (first_ty, first_shape, _) = declared(first.field);
                    if first_ty != ty || first_shape != shape {
                        let full = |field: &Field<'_>| {
                            let ty = field.ty;
                            quote!(#ty).to_string()
                        };
                        return Err(refuse(
                            field.ident,
                            format_args!(
                                "datum `{name}` is declared as `{}` in `{}` and as `{}` in `{}`; a datum has one type and attribute in every variant carrying it, or is `T` in some and `Option<T>` in others",
                                full(first.field),
                                first.variant,
                                full(field),
                                variant.ident
                            ),
                        ));
                    }
                    datum.decls.push(decl);
                }
                None => {
                    let index = datums.len();
                    datums.push(Datum { name, decls: vec![decl], index });
                }
            }
        }
    }
    Ok(datums)
}

/// The tag guards of `variants` over `datums`: one per variant, naming the datums it carries and
/// whether as `T` (required).
fn tag_guards(variants: &[Variant<'_>], datums: &[Datum<'_, '_>], tag: &TokenStream) -> Vec<TokenStream> {
    variants
        .iter()
        .map(|variant| {
            let code = code_expr(&variant.code);
            let carried = datums.iter().filter_map(|datum| {
                let decl = datum.decls.iter().find(|decl| decl.variant == variant.ident)?;
                let (index, required) = (datum.index, !decl.optional);
                Some(quote!((#index, #required)))
            });
            quote!(layout.tag_guard(#tag, #code, &datums, &[#(#carried),*]);)
        })
        .collect()
}

/// The arms constructing each variant from the datums' locals, by code (`code.as_str()`).
fn construct_arms(variants: &[Variant<'_>], datums: &[Datum<'_, '_>]) -> Vec<TokenStream> {
    let schema = schema();
    variants
        .iter()
        .map(|variant| {
            let (variant_ident, code) = (variant.ident, &variant.code);
            match &variant.fields {
                None => quote!(#code => Self::#variant_ident),
                Some(fields) => {
                    let values = fields.iter().map(|field| {
                        let datum = datums.iter().find(|datum| datum.name == field.name.value()).expect("every field is a datum");
                        let index = datum.decl_of(variant.ident).expect("declared");
                        let (field_ident, local, name) = (field.ident, datum.local(index), &field.name);
                        quote!(#field_ident: #local.ok_or_else(|| #schema::DecodeError::missing(#name))?)
                    });
                    quote!(#code => Self::#variant_ident { #(#values,)* })
                }
            }
        })
        .collect()
}

fn data_enum(input: &DeriveInput, variants: &[Variant<'_>], codes: &[(String, String)]) -> syn::Result<TokenStream> {
    let schema = schema();
    let ident = &input.ident;
    let datums = datums(variants, false)?;
    let all_fields = variants.iter().flat_map(|variant| variant.fields.iter().flatten());
    let generics = Generics::new(&input.generics, all_fields.map(Field::bound));
    let Generics { impl_generics, ty_generics, where_clause } = &generics;
    let element = columns_element(ident, &generics);
    let tables = datums.iter().map(|datum| datum.field().tables());
    let arms = code_arms(variants);
    let all = codes_expr(codes);
    let names = names_expr(codes);

    let layout = datums.iter().map(|datum| datum.layout(&quote!(name), |f| quote!(layout.scope(#f))));
    let guards = tag_guards(variants, &datums, &quote!(tag));
    let encode = datums.iter().map(|datum| datum.encode(&quote!(self)));
    let encode_absent = datums.iter().map(|datum| datum.field().encode_absent());
    let decode = datums.iter().map(|datum| datum.decode(&quote!(code)));
    let construct = construct_arms(variants, &datums);
    let decode_absent: Vec<TokenStream> = datums.iter().map(|datum| datum.field().decode_absent()).collect();

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics #schema::Columns for #ident #ty_generics #where_clause {
            const TABLES: usize = 0 #(+ #tables)*;

            fn layout(layout: &mut #schema::Layout, name: &str, required: bool) {
                let tag = if name.is_empty() { "tag" } else { name };
                const CODES: &[#schema::Code] = #all;
                layout.code(tag, CODES, #names, required);
                let datums: ::std::vec::Vec<#schema::DatumColumns> = ::std::vec![#(#layout),*];
                #(#guards)*
                layout.expansion(tag, &datums);
            }

            #[allow(irrefutable_let_patterns)]
            fn encode(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                let tag: #schema::Code = match self {
                    #(#arms,)*
                };
                e.put(#schema::code_value(tag));
                #(#encode)*
                ::core::result::Result::Ok(())
            }

            fn encode_absent(e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                e.put(#schema::Value::Null);
                #(#encode_absent)*
                ::core::result::Result::Ok(())
            }

            fn decode(d: &mut #schema::Decoder) -> ::core::result::Result<::core::option::Option<Self>, #schema::DecodeError> {
                const CODES: &[#schema::Code] = #all;
                let ::core::option::Option::Some(code) = #schema::decode_code(d, CODES)? else {
                    #(#decode_absent)*
                    return ::core::result::Result::Ok(::core::option::Option::None);
                };
                #(#decode)*
                ::core::result::Result::Ok(::core::option::Option::Some(match code.as_str() {
                    #(#construct,)*
                    _ => return ::core::result::Result::Err(#schema::DecodeError::Code { column: ::std::string::String::new(), code: code.to_string() }),
                }))
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
    })
}

/// A machine's state enum (`#[store(labels = L)]`): `Labelled` with label `L`, and `StateData`,
/// which `State<Self>` stores after the label column and `<field>_since`.
fn labels_enum(input: &DeriveInput, labels: &syn::Path, variants: &[Variant<'_>], codes: &[(String, String)]) -> syn::Result<TokenStream> {
    let schema = schema();
    let ident = &input.ident;
    let datums = datums(variants, true)?;
    let all_fields = variants.iter().flat_map(|variant| variant.fields.iter().flatten());
    let generics = Generics::new(&input.generics, all_fields.map(Field::bound));
    let Generics { impl_generics, ty_generics, where_clause } = &generics;
    let names: Vec<&syn::LitStr> = datums.iter().map(|datum| &datum.decls[0].field.name).collect();
    let tables = datums.iter().map(|datum| datum.field().tables());
    let arms = code_arms(variants);
    let all = codes_expr(codes);
    let label_names = names_expr(codes);
    let label_arms = variants.iter().map(|variant| {
        let variant_ident = variant.ident;
        match variant.fields {
            None => quote!(Self::#variant_ident => &<#labels>::#variant_ident),
            Some(_) => quote!(Self::#variant_ident { .. } => &<#labels>::#variant_ident),
        }
    });

    // Each label of `L` is a variant with the same code: the label column stores the variant's
    // code, and indexes and edge events name `L`'s.
    let agree = variants.iter().map(|variant| {
        let (variant_ident, code) = (variant.ident, code_expr(&variant.code));
        let message = format!("label `{variant_ident}` has another code in the labels enum than `{ident}::{variant_ident}`");
        quote!(::core::assert_eq!(<#labels as #schema::CodeEnum>::code(&<#labels>::#variant_ident), #code, #message);)
    });
    let same_labels = format!("the labels enum of `{ident}` has other labels than its variants");
    // A column datum spans no table, and a collection datum's rows hold no tables of their own:
    // edges edit the state's data one row at a time.
    let shapes = datums.iter().map(|datum| {
        let via = datum.field().via();
        if datum.is_table() {
            let message = format!("state datum `{}`: a collection's elements hold no tables", datum.name);
            quote!(const { ::core::assert!(#via::TABLES == 1, #message) };)
        } else {
            let message = format!("state datum `{}`: a column datum holds no tables", datum.name);
            quote!(const { ::core::assert!(#via::TABLES == 0, #message) };)
        }
    });
    let layout = datums.iter().map(|datum| {
        let name = &datum.decls[0].field.name;
        datum.layout(&quote!(""), |f| quote!(layout.datum(#name, label, #f)))
    });
    let guards = tag_guards(variants, &datums, &quote!(label));

    let encode = datums.iter().map(|datum| datum.encode(&quote!(self)));
    let encode_absent = datums.iter().map(|datum| datum.field().encode_absent());
    let decode = datums.iter().map(|datum| datum.decode(&quote!(code)));
    let construct = construct_arms(variants, &datums);
    let decode_absent = datums.iter().map(|datum| datum.field().decode_absent());
    let delta = datums.iter().map(|datum| {
        let name = &datum.decls[0].field.name;
        let encode = datum.encode(&quote!(value));
        let method = if datum.is_table() { quote!(table) } else { quote!(column) };
        quote! {
            delta.#method(
                #name,
                |value: &Self, e: &mut #schema::Encoder| -> ::core::result::Result<(), #schema::EncodeError> {
                    #encode
                    ::core::result::Result::Ok(())
                },
                previous,
                next,
            )?;
        }
    });

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics #schema::Labelled for #ident #ty_generics #where_clause {
            type Label = #labels;

            fn label(&self) -> &#labels {
                match self {
                    #(#label_arms,)*
                }
            }
        }

        #[automatically_derived]
        impl #impl_generics #schema::StateData for #ident #ty_generics #where_clause {
            const TABLES: usize = 0 #(+ #tables)*;
            const CODES: &'static [#schema::Code] = #all;
            const NAMES: &'static [&'static str] = #label_names;
            const DATA: &'static [&'static str] = &[#(#names),*];

            fn code(&self) -> #schema::Code {
                match self {
                    #(#arms,)*
                }
            }

            fn layout_data(layout: &mut #schema::Layout, label: &str) {
                #(#shapes)*
                ::core::assert_eq!(<#labels as #schema::CodeEnum>::CODES, <Self as #schema::StateData>::CODES, #same_labels);
                #(#agree)*
                let datums: ::std::vec::Vec<#schema::DatumColumns> = ::std::vec![#(#layout),*];
                #(#guards)*
            }

            fn encode_data(&self, e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                #(#encode)*
                ::core::result::Result::Ok(())
            }

            fn encode_absent_data(e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                #(#encode_absent)*
                ::core::result::Result::Ok(())
            }

            fn decode_data(code: #schema::Code, d: &mut #schema::Decoder) -> ::core::result::Result<Self, #schema::DecodeError> {
                #(#decode)*
                ::core::result::Result::Ok(match code.as_str() {
                    #(#construct,)*
                    _ => return ::core::result::Result::Err(#schema::DecodeError::Code { column: ::std::string::String::new(), code: code.to_string() }),
                })
            }

            fn decode_absent_data(d: &mut #schema::Decoder) -> ::core::result::Result<(), #schema::DecodeError> {
                #(#decode_absent)*
                ::core::result::Result::Ok(())
            }

            fn delta(previous: &Self, next: &Self, delta: &mut #schema::StateDelta) -> ::core::result::Result<(), #schema::EncodeError> {
                #(#delta)*
                ::core::result::Result::Ok(())
            }
        }
    })
}
