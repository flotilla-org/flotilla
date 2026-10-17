//! Code generation for enums: unit code enums and data enums.
//!
//! Every variant carries an explicit `#[store(code = N)]`; serde names and declaration order never
//! reach storage. An enum whose variants are all units is a code enum: one `INTEGER` column with
//! `CHECK (c IN (codes))`, implementing `CodeEnum`. An enum with a named-field variant is a data
//! enum: the same tag column, then each data variant's fields as sparse columns
//! `<field>_<variant>_<subfield>` inside a layout guard on the tag, which emits that variant's one
//! CHECK. Unit variants of a data enum have no columns and so no guard. A variant field holding a
//! collection is a nested table `<table>_<field>_<variant>_<subfield>`, which has rows only while
//! the variant is active.
//!
//! An enum with `#[store(labels = L)]` is a machine's state (#3373 ruling 1): its variants are the
//! labels of the code enum `L`, code for code, and their fields the state's data. It is stored only
//! under `#[store(state)]`, as `State<Self>`: one label column, `<field>_since`, then one column
//! group or table per datum, shared by name across the variants that carry it, and one CHECK per
//! label requiring the label's own data and nulling the data it lacks. A datum is unattributed (its
//! columns), `child` (a `BTreeMap` or `BTreeSet`: a table keyed by owner plus map key or set value,
//! changed row by row), `intern`, or `flatten` (a struct without table fields as columns
//! `<datum>_<field>`, or an `Option` of one with its `<datum>_present` flag, which is null under a
//! label lacking the datum); it has the same type and attribute in every variant carrying it. A datum's table holds rows only while the label carries it, and decoding refuses rows
//! otherwise.

use std::collections::BTreeMap;

use proc_macro2::TokenStream;
use quote::quote;
use syn::{DataEnum, DeriveInput, Fields, Ident};

use crate::{
    attrs::{self, refuse, refuse_at, TypeAttrs},
    fields::{columns_element, schema, Field, Generics},
    shape::{Collection, FieldShape},
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
        return Err(refuse(index, "`index` belongs on a kind or a part struct"));
    }
    if let Some(edges) = &type_attrs.edges {
        return Err(refuse(edges, "`edges` belongs on a kind, whose log entries it constrains"));
    }
    if data.variants.is_empty() {
        return Err(refuse(&input.ident, "an enum without variants has no values to store"));
    }
    let labels = type_attrs.labels.as_ref();
    let mut seen: BTreeMap<i64, &Ident> = BTreeMap::new();
    let mut variants = Vec::new();
    for variant in &data.variants {
        let code = attrs::variant_code(&variant.attrs)?;
        let fields = match &variant.fields {
            Fields::Unnamed(fields) => return Err(refuse(fields, "tuple variants cannot be stored; use named fields")),
            Fields::Unit => None,
            // A label's data is named by the datum alone: shared by name across the labels.
            Fields::Named(fields) if labels.is_some() => Some(
                fields
                    .named
                    .iter()
                    .map(|field| Field::parse(field, &input.ident, None).map(|field| Field { state_datum: true, ..field }))
                    .collect::<syn::Result<Vec<_>>>()?,
            ),
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
    if let Some(labels) = labels {
        return labels_enum(input, labels, &variants, &codes);
    }
    // `A { b_c }` and `AB { c }` would both name the column `a_b_c`; refuse here with a span
    // rather than leave it to the descriptor's duplicate-column check at runtime.
    let mut columns: BTreeMap<String, &Ident> = BTreeMap::new();
    for variant in &data.variants {
        if let Fields::Named(fields) = &variant.fields {
            let prefix = snake_case(&variant.ident.to_string());
            for field in fields.named.iter().filter_map(|field| field.ident.as_ref()) {
                let column = format!("{prefix}_{field}");
                if let Some(previous) = columns.insert(column.clone(), &variant.ident) {
                    if previous != &variant.ident {
                        return Err(refuse(
                            &variant.ident,
                            format_args!("variants `{previous}` and `{}` both name the column `{column}`", variant.ident),
                        ));
                    }
                }
            }
        }
    }
    if variants.iter().all(|variant| variant.fields.is_none()) {
        Ok(code_enum(input, &variants, &codes))
    } else {
        Ok(data_enum(input, &variants, &codes))
    }
}

/// `TerminalSession` becomes `terminal_session` and `HTTPRoute` becomes `http_route`. The same
/// rule as `flotilla_schema::snake_case`, which names the columns; the schema crate's
/// `variant_columns_use_the_schema_snake_case` test pins that the two agree.
fn snake_case(name: &str) -> String {
    let characters: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (index, &character) in characters.iter().enumerate() {
        if character.is_ascii_uppercase() {
            let previous = index.checked_sub(1).map(|index| characters[index]);
            let next = characters.get(index + 1);
            let after_word = previous.is_some_and(|previous| previous.is_ascii_lowercase() || previous.is_ascii_digit());
            let ends_acronym = previous.is_some_and(|previous| previous.is_ascii_uppercase()) && next.is_some_and(char::is_ascii_lowercase);
            if after_word || ends_acronym {
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
    let state_arms = variants.iter().map(|variant| {
        let (ident, code) = (variant.ident, variant.code);
        quote!(#code => ::core::result::Result::Ok(Self::#ident))
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
        impl #impl_generics #schema::KeyColumn for #ident #ty_generics #where_clause {}

        #[automatically_derived]
        impl #impl_generics #schema::CodeEnum for #ident #ty_generics #where_clause {
            const CODES: &'static [i64] = &[#(#codes),*];

            fn code(&self) -> i64 {
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
            const CODES: &'static [i64] = &[#(#codes),*];
            const DATA: &'static [&'static str] = &[];

            fn code(&self) -> i64 {
                <Self as #schema::CodeEnum>::code(self)
            }

            fn layout_data(_layout: &mut #schema::Layout, _label: &str) {}

            fn encode_data(&self, _e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                ::core::result::Result::Ok(())
            }

            fn encode_absent_data(_e: &mut #schema::Encoder) -> ::core::result::Result<(), #schema::EncodeError> {
                ::core::result::Result::Ok(())
            }

            fn decode_data(code: i64, _d: &mut #schema::Decoder) -> ::core::result::Result<Self, #schema::DecodeError> {
                match code {
                    #(#state_arms,)*
                    code => ::core::result::Result::Err(#schema::DecodeError::Code { column: ::std::string::String::new(), code }),
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

/// One datum of a label-plus-data state: its first declaration, and the labels carrying it.
struct Datum<'v, 'a> {
    field: &'v Field<'a>,
    /// The carrying labels' variants, in declaration order.
    variants: Vec<&'a Ident>,
    codes: Vec<i64>,
    /// Its index among the state's datums.
    index: usize,
}

impl Datum<'_, '_> {
    /// `Option<&Datum>` of `value` (an expression of type `&Self`): `None` under a label lacking it.
    fn of(&self, value: &TokenStream) -> TokenStream {
        let (ident, local) = (self.field.ident, &self.field.local);
        let carriers = self.variants.iter().map(|variant| quote!(Self::#variant { #ident: #local, .. }));
        quote! {
            match #value {
                #(#carriers)|* => ::core::option::Option::Some(#local),
                #[allow(unreachable_patterns)]
                _ => ::core::option::Option::None,
            }
        }
    }

    fn is_table(&self) -> bool {
        self.field.is_table()
    }
}

/// The datums of `variants`, in order of first declaration, refusing a datum outside the storage
/// forms a state datum takes, and one declared with different types or attributes.
fn datums<'v, 'a>(variants: &'v [Variant<'a>]) -> syn::Result<Vec<Datum<'v, 'a>>> {
    let mut datums: Vec<Datum<'v, 'a>> = Vec::new();
    for variant in variants {
        for field in variant.fields.iter().flatten() {
            let name = field.name.value();
            match field.shape {
                FieldShape::Column
                | FieldShape::Intern { .. }
                | FieldShape::Flatten { .. }
                | FieldShape::Child(Collection::Map | Collection::Set) => {}
                _ => {
                    return Err(refuse(
                        field.ident,
                        format_args!(
                            "state datum `{name}` takes no attribute (its columns), `child` (a BTreeMap or BTreeSet, changed row by row), `intern` or `flatten` (a struct without table fields, or an Option of one)"
                        ),
                    ));
                }
            }
            let ty = |field: &Field<'_>| {
                let ty = field.ty;
                quote!(#ty).to_string()
            };
            match datums.iter_mut().find(|datum| datum.field.name.value() == name) {
                Some(datum) => {
                    if ty(datum.field) != ty(field) || datum.field.shape != field.shape {
                        return Err(refuse(
                            field.ident,
                            format_args!(
                                "state datum `{name}` is declared as `{}` in `{}` and as `{}` in `{}`; a datum has one type and attribute in every label carrying it",
                                ty(datum.field),
                                datum.variants[0],
                                ty(field),
                                variant.ident
                            ),
                        ));
                    }
                    datum.variants.push(variant.ident);
                    datum.codes.push(variant.code);
                }
                None => {
                    let index = datums.len();
                    datums.push(Datum { field, variants: vec![variant.ident], codes: vec![variant.code], index });
                }
            }
        }
    }
    Ok(datums)
}

/// A machine's state enum (`#[store(labels = L)]`): `Labelled` with label `L`, and `StateData`,
/// which `State<Self>` stores after the label column and `<field>_since`.
fn labels_enum(input: &DeriveInput, labels: &syn::Path, variants: &[Variant<'_>], codes: &[i64]) -> syn::Result<TokenStream> {
    let schema = schema();
    let ident = &input.ident;
    let datums = datums(variants)?;
    let generics = Generics::new(&input.generics, datums.iter().map(|datum| datum.field.bound()));
    let Generics { impl_generics, ty_generics, where_clause } = &generics;
    let names: Vec<&syn::LitStr> = datums.iter().map(|datum| &datum.field.name).collect();
    let tables = datums.iter().map(|datum| datum.field.tables());
    let arms = code_arms(variants);
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
        let (variant_ident, code) = (variant.ident, variant.code);
        let message = format!("label `{variant_ident}` has another code in the labels enum than `{ident}::{variant_ident}`");
        quote!(::core::assert_eq!(<#labels as #schema::CodeEnum>::code(&<#labels>::#variant_ident), #code, #message);)
    });
    let same_labels = format!("the labels enum of `{ident}` has other labels than its variants");
    // A column datum spans no table, and a collection datum's rows hold no tables of their own:
    // edges edit the state's data one row at a time.
    let shapes = datums.iter().map(|datum| {
        let via = datum.field.via();
        if datum.is_table() {
            let message = format!("state datum `{}`: a collection's elements hold no tables", datum.field.name.value());
            quote!(const { ::core::assert!(#via::TABLES == 1, #message) };)
        } else {
            let message = format!("state datum `{}`: a column datum holds no tables", datum.field.name.value());
            quote!(const { ::core::assert!(#via::TABLES == 0, #message) };)
        }
    });
    let layout = datums.iter().map(|datum| {
        let (name, layout) = (&datum.field.name, datum.field.layout(&quote!(""), &quote!(true)));
        quote!(layout.datum(#name, label, |layout| { #layout }))
    });
    let guards = variants.iter().map(|variant| {
        let code = variant.code;
        let carried = datums.iter().filter(|datum| datum.codes.contains(&code)).map(|datum| datum.index);
        quote!(layout.label_guard(label, #code, &datums, &[#(#carried),*]);)
    });

    let encode = datums.iter().map(|datum| {
        let of = datum.of(&quote!(self));
        let (present, absent) = (datum.field.encode(&quote!(value)), datum.field.encode_absent());
        quote! {
            match #of {
                ::core::option::Option::Some(value) => { #present }
                ::core::option::Option::None => { #absent }
            }
        }
    });
    let encode_absent = datums.iter().map(|datum| datum.field.encode_absent());
    let decode = datums.iter().map(|datum| {
        let (via, name, local, carried) = (datum.field.via(), &datum.field.name, &datum.field.local, &datum.codes);
        quote! {
            let #local = if [#(#carried),*].contains(&code) {
                ::core::option::Option::Some(
                    #via::decode(d).map_err(|error| error.at(#name))?.ok_or_else(|| #schema::DecodeError::missing(#name))?,
                )
            } else {
                #via::decode_absent(d).map_err(|error| error.at(#name))?;
                ::core::option::Option::None
            };
        }
    });
    let construct = variants.iter().map(|variant| {
        let (variant_ident, code) = (variant.ident, variant.code);
        match &variant.fields {
            None => quote!(#code => ::core::result::Result::Ok(Self::#variant_ident)),
            Some(fields) => {
                let values = fields.iter().map(|field| {
                    let (field_ident, local, name) = (field.ident, &field.local, &field.name);
                    quote!(#field_ident: #local.ok_or_else(|| #schema::DecodeError::missing(#name))?)
                });
                quote!(#code => ::core::result::Result::Ok(Self::#variant_ident { #(#values,)* }))
            }
        }
    });
    let decode_absent = datums.iter().map(|datum| datum.field.decode_absent());
    let delta = datums.iter().map(|datum| {
        let (via, name) = (datum.field.via(), &datum.field.name);
        let (previous, next) = (datum.of(&quote!(previous)), datum.of(&quote!(next)));
        let method = if datum.is_table() { quote!(table) } else { quote!(column) };
        quote!(delta.#method(#name, #via::encode, #via::encode_absent, #previous, #next)?;)
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
            const CODES: &'static [i64] = &[#(#codes),*];
            const DATA: &'static [&'static str] = &[#(#names),*];

            fn code(&self) -> i64 {
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

            fn decode_data(code: i64, d: &mut #schema::Decoder) -> ::core::result::Result<Self, #schema::DecodeError> {
                #(#decode)*
                match code {
                    #(#construct,)*
                    code => ::core::result::Result::Err(#schema::DecodeError::Code { column: ::std::string::String::new(), code }),
                }
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
