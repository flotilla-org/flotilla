//! `#[store(index(...))]` on a kind or a kind's spec: the descriptor's `Index` on its root table
//! and a typed query constructor; on a spec, `state` names the holder's state field (#3392 D5). On a part struct: an `Index` on each table holding its rows (an edge log's
//! `EdgeEvent`), with no constructor; reads of those tables are the history views' to define.
//!
//! ```text
//! index(name = "open_by_inbox", eq(inbox), order(accepted_seq), when(state in [Pending, Awaiting]))
//! index(name = "deadline", order(deadline), when(state in [Pending, Awaiting], deadline is_some))
//! ```
//!
//! `eq` and `order` name top-level single-column fields: unattributed fields whose type (inside
//! `Option`) is a `KeyColumn`, or `intern` fields. `order` refuses `intern` fields and references
//! (`Ref`, `Identity`): their columns hold local surrogate ids, whose order is insertion order in one
//! store and differs between stores, so it is no order a caller could mean. `when` conjoins `<state field> in [Variant, ...]`,
//! whose codes come from the state's label enum (`Labelled::Label`), and `<optional field> is_some`. An optional
//! `order` field must be required by `is_some`, so keyset paging never compares a null.

use std::collections::BTreeSet;

use proc_macro2::TokenStream;
use quote::{format_ident, quote, quote_spanned};
use syn::{
    bracketed,
    ext::IdentExt,
    parenthesized,
    parse::{ParseStream, Parser},
    punctuated::Punctuated,
    DeriveInput, Ident, LitStr, Token, Type,
};

use crate::{
    attrs::{refuse, refuse_at},
    fields::{schema, Field, Generics},
    shape::{self, FieldShape},
};

struct IndexAttr {
    name: LitStr,
    eq: Vec<Ident>,
    order: Vec<Ident>,
    when: Vec<Condition>,
}

enum Condition {
    In { field: Ident, variants: Vec<Ident> },
    IsSome { field: Ident },
}

/// A field an index names, resolved against the kind's fields.
struct Column<'f, 'a> {
    field: &'f Field<'a>,
    /// The argument and key type: the field type, inside `Option` when the field is optional.
    inner: &'a Type,
    optional: bool,
    interned: bool,
}

/// Parses every `index(...)` body and checks it against the kind's fields.
pub(crate) struct Indexes<'f, 'a> {
    indexes: Vec<(IndexAttr, Key<'f, 'a>)>,
    fields: &'f [Field<'a>],
    /// A kind spec's `state = S`: `state in [...]` names the holder's state field, `State<S>`.
    holder_state: Option<&'f Type>,
}

impl<'f, 'a> Indexes<'f, 'a> {
    pub(crate) fn parse(bodies: &[TokenStream], fields: &'f [Field<'a>], holder_state: Option<&'f Type>) -> syn::Result<Indexes<'f, 'a>> {
        let mut indexes = Vec::new();
        let mut names = BTreeSet::new();
        for body in bodies {
            let index = parse_body.parse2(body.clone())?;
            if !names.insert(index.name.value()) {
                return Err(refuse(&index.name, format_args!("index `{}` is declared twice", index.name.value())));
            }
            let key = check(&index, fields, holder_state.is_some())?;
            indexes.push((index, key));
        }
        Ok(Indexes { indexes, fields, holder_state })
    }

    /// Statements for `table_set`, after the root's layout: one `layout.index(..)` per index.
    pub(crate) fn layout(&self) -> TokenStream {
        let schema = schema();
        let indexes = self.indexes.iter().map(|(index, (eq, order))| {
            let name = &index.name;
            let eq = eq.iter().map(|column| &column.field.name);
            let order = order.iter().map(|column| &column.field.name);
            let predicate = index.when.iter().map(|condition| match condition {
                Condition::In { field, variants } => {
                    // `check` resolved this field and refused it unless it is a `State<E>` field,
                    // or the holder's state of a kind spec.
                    let (column, state) = match (self.holder_state, find(self.fields, field)) {
                        (Some(state), _) if field.unraw() == HOLDER_STATE => (LitStr::new(HOLDER_STATE, field.span()), state),
                        (_, field) => {
                            let field = field.expect("checked");
                            (field.name.clone(), shape::first_arg(field.ty).expect("checked State<E>"))
                        }
                    };
                    let column = &column;
                    // The codes come from the state's label enum (the code enum itself, or a
                    // label-plus-data state's `labels` enum), never restated. Each code expression
                    // carries its variant's span, so a label the enum lacks is reported at the
                    // attribute that names it.
                    let label = quote!(<#state as #schema::Labelled>::Label);
                    let codes = variants
                        .iter()
                        .map(|variant| quote_spanned!(variant.span()=> <#label as #schema::CodeEnum>::code(&<#label>::#variant)));
                    quote! {
                        #schema::Condition::In {
                            column: ::std::string::String::from(#column),
                            codes: ::std::vec![#(#codes),*],
                        }
                    }
                }
                Condition::IsSome { field } => {
                    // `check` resolved this field as an optional single column.
                    let column = &find(self.fields, field).expect("checked").name;
                    quote!(#schema::Condition::NotNull { column: ::std::string::String::from(#column) })
                }
            });
            quote! {
                layout.index(#schema::Index {
                    name: ::std::string::String::from(#name),
                    table: ::std::string::String::from(layout.table_name()),
                    eq: ::std::vec![#(::std::string::String::from(#eq)),*],
                    order: ::std::vec![#(::std::string::String::from(#order)),*],
                    predicate: ::std::vec![#(#predicate),*],
                });
            }
        });
        quote!(#(#indexes)*)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.indexes.is_empty()
    }

    /// The inherent impl holding one query constructor per index, named after it.
    pub(crate) fn constructors(&self, input: &DeriveInput) -> TokenStream {
        if self.indexes.is_empty() {
            return TokenStream::new();
        }
        let schema = schema();
        let ident = &input.ident;
        let (_, ty_generics, _) = input.generics.split_for_impl();
        let declares = if self.holder_state.is_some() { quote!(#schema::KindSpec) } else { quote!(#schema::Kind) };
        let mut bounds = vec![quote!(#ident #ty_generics: #declares)];
        let mut functions = Vec::new();
        for (index, (eq, order)) in &self.indexes {
            for column in eq.iter().chain(order).filter(|column| !column.interned) {
                let inner = column.inner;
                bounds.push(quote!(#inner: #schema::KeyColumn + #schema::Element));
            }
            let function = format_ident!("{}", index.name.value(), span = index.name.span());
            let name = &index.name;
            let args = eq.iter().map(|column| {
                let (arg, inner) = (column.field.ident, column.inner);
                quote!(#arg: &#inner)
            });
            let values = eq.iter().map(|column| {
                let (arg, inner) = (column.field.ident, column.inner);
                if column.interned {
                    quote!(#schema::interned_value::<#inner>(#arg))
                } else {
                    quote!(#schema::key_value::<#inner>(#arg)?)
                }
            });
            let doc = format!(
                "Rows of index `{}`: {}ordered by {}.",
                index.name.value(),
                if eq.is_empty() { String::new() } else { format!("{} equal to the arguments, ", names(eq)) },
                if order.is_empty() { "id".to_owned() } else { format!("{}, then id", names(order)) },
            );
            functions.push(quote! {
                #[doc = #doc]
                pub fn #function(#(#args),*) -> #schema::IndexQuery<Self> {
                    let values = || -> ::core::result::Result<::std::vec::Vec<#schema::Value>, #schema::EncodeError> {
                        ::core::result::Result::Ok(::std::vec![#(#values),*])
                    };
                    #schema::IndexQuery::new(<Self as #declares>::CODE, #name, values())
                }
            });
        }
        let Generics { impl_generics, ty_generics, where_clause } = Generics::new(&input.generics, bounds);
        quote! {
            #[automatically_derived]
            impl #impl_generics #ident #ty_generics #where_clause {
                #(#functions)*
            }
        }
    }
}

fn names(columns: &[Column<'_, '_>]) -> String {
    columns.iter().map(|column| format!("`{}`", column.field.name.value())).collect::<Vec<_>>().join(", ")
}

fn parse_body(input: ParseStream<'_>) -> syn::Result<IndexAttr> {
    let mut name: Option<LitStr> = None;
    let mut eq: Option<Vec<Ident>> = None;
    let mut order: Option<Vec<Ident>> = None;
    let mut when: Option<Vec<Condition>> = None;
    let span = input.span();
    while !input.is_empty() {
        let key = Ident::parse_any(input)?;
        let twice = || refuse(&key, format_args!("index argument `{key}` is given twice"));
        match key.to_string().as_str() {
            "name" => {
                input.parse::<Token![=]>()?;
                let value: LitStr = input.parse()?;
                let text = value.value();
                let valid = text.starts_with(|c: char| c.is_ascii_lowercase())
                    && text.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                if !valid {
                    return Err(refuse(&value, "an index name is snake_case ASCII, such as \"open_by_inbox\""));
                }
                if name.replace(value).is_some() {
                    return Err(twice());
                }
            }
            "eq" | "order" => {
                let content;
                parenthesized!(content in input);
                let fields: Vec<Ident> =
                    Punctuated::<Ident, Token![,]>::parse_terminated_with(&content, Ident::parse_any)?.into_iter().collect();
                let slot = if key == "eq" { &mut eq } else { &mut order };
                if slot.replace(fields).is_some() {
                    return Err(twice());
                }
            }
            "when" => {
                let content;
                parenthesized!(content in input);
                let conditions =
                    Punctuated::<Condition, Token![,]>::parse_terminated_with(&content, parse_condition)?.into_iter().collect();
                if when.replace(conditions).is_some() {
                    return Err(twice());
                }
            }
            other => return Err(refuse(&key, format_args!("unknown index argument `{other}`; an index takes name, eq, order and when"))),
        }
        if !input.is_empty() {
            input.parse::<Token![,]>()?;
        }
    }
    let name = name.ok_or_else(|| refuse_at(span, "an index needs `name = \"...\"`"))?;
    Ok(IndexAttr { name, eq: eq.unwrap_or_default(), order: order.unwrap_or_default(), when: when.unwrap_or_default() })
}

fn parse_condition(input: ParseStream<'_>) -> syn::Result<Condition> {
    let field = Ident::parse_any(input)?;
    if input.peek(Token![in]) {
        input.parse::<Token![in]>()?;
        let content;
        bracketed!(content in input);
        let variants: Vec<Ident> = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?.into_iter().collect();
        if variants.is_empty() {
            return Err(refuse(&field, "`in [...]` lists at least one variant"));
        }
        return Ok(Condition::In { field, variants });
    }
    let test = Ident::parse_any(input)?;
    if test != "is_some" {
        return Err(refuse(&test, "an index condition is `<state field> in [Variant, ...]` or `<optional field> is_some`"));
    }
    Ok(Condition::IsSome { field })
}

fn find<'f, 'a>(fields: &'f [Field<'a>], ident: &Ident) -> Option<&'f Field<'a>> {
    fields.iter().find(|field| field.ident.unraw() == ident.unraw())
}

fn field<'f, 'a>(fields: &'f [Field<'a>], ident: &Ident) -> syn::Result<&'f Field<'a>> {
    find(fields, ident).ok_or_else(|| {
        let known: Vec<String> = fields.iter().map(|field| field.ident.unraw().to_string()).collect();
        refuse(ident, format_args!("index names `{ident}`, which is not a field of this struct (fields: {})", known.join(", ")))
    })
}

/// A field usable as an index column: one column of the root row.
fn column<'f, 'a>(fields: &'f [Field<'a>], ident: &Ident) -> syn::Result<Column<'f, 'a>> {
    let field = field(fields, ident)?;
    let column = match field.shape {
        FieldShape::Column => match shape::option_inner(field.ty) {
            Some(inner) => Column { field, inner, optional: true, interned: false },
            None => Column { field, inner: field.ty, optional: false, interned: false },
        },
        FieldShape::Intern { optional } => {
            let inner = if optional { shape::option_inner(field.ty).unwrap_or(field.ty) } else { field.ty };
            Column { field, inner, optional, interned: true }
        }
        _ => {
            return Err(refuse(
                ident,
                format_args!("index column `{ident}` must be a single-column field: an unattributed key column or an `intern` field"),
            ))
        }
    };
    Ok(column)
}

/// An index's `eq` and `order` columns.
type Key<'f, 'a> = (Vec<Column<'f, 'a>>, Vec<Column<'f, 'a>>);

/// The name a kind spec's indexes give the holder's state field.
const HOLDER_STATE: &str = "state";

fn check<'f, 'a>(index: &IndexAttr, fields: &'f [Field<'a>], holder_state: bool) -> syn::Result<Key<'f, 'a>> {
    if index.eq.is_empty() && index.order.is_empty() {
        return Err(refuse(&index.name, "an index needs at least one `eq` or `order` field"));
    }
    let eq = index.eq.iter().map(|ident| column(fields, ident)).collect::<syn::Result<Vec<_>>>()?;
    let order = index.order.iter().map(|ident| column(fields, ident)).collect::<syn::Result<Vec<_>>>()?;
    // Spans point at the name inside `order(...)`, where the fix is made.
    for (ident, column) in index.order.iter().zip(&order) {
        let reference = shape::head_name(column.inner).is_some_and(|name| name == "Ref" || name == "Identity");
        if column.interned || reference {
            return Err(refuse(
                ident,
                format_args!(
                    "`{ident}` cannot order an index: interned strings and references are stored as local ids, \
                     whose order differs between stores; use it in `eq`"
                ),
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for ident in index.eq.iter().chain(&index.order) {
        if !seen.insert(ident.unraw().to_string()) {
            return Err(refuse(ident, format_args!("index column `{ident}` is named twice")));
        }
    }
    let mut required = BTreeSet::new();
    for condition in &index.when {
        match condition {
            Condition::In { field: ident, variants } => {
                if !(holder_state && ident.unraw() == HOLDER_STATE) {
                    let field = field(fields, ident)?;
                    if field.shape != FieldShape::State {
                        return Err(refuse(ident, format_args!("`{ident} in [...]` needs a `#[store(state)]` field")));
                    }
                }
                let mut seen = BTreeSet::new();
                if let Some(variant) = variants.iter().find(|variant| !seen.insert(variant.to_string())) {
                    return Err(refuse(variant, format_args!("variant `{variant}` is listed twice")));
                }
            }
            Condition::IsSome { field: ident } => {
                if !column(fields, ident)?.optional {
                    return Err(refuse(ident, format_args!("`{ident} is_some` needs an optional single-column field")));
                }
                required.insert(ident.unraw().to_string());
            }
        }
    }
    if let Some(column) = order.iter().find(|column| column.optional && !required.contains(&column.field.ident.unraw().to_string())) {
        let ident = column.field.ident;
        return Err(refuse(
            ident,
            format_args!("optional order field `{ident}` needs `when({ident} is_some)`: paging cannot compare a null"),
        ));
    }
    Ok((eq, order))
}
