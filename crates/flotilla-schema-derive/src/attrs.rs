//! Parsing `#[store(...)]`: the closed attribute list, and nothing else.

use proc_macro2::{Span, TokenStream};
use quote::ToTokens;
use syn::{parenthesized, Attribute, LitInt, LitStr};

/// The closed attribute list, named by every refusal. `flotilla-schema` repeats this text in its
/// trait diagnostics and a test there keeps the two equal.
pub(crate) const SUPPORTED: &str = "the storage derive supports only the attributes child, records, history, frozen, cbor, intern, \
                                    flatten, state, alias and code (fields and variants) and kind, code, index, labels and edges (types)";

/// A refusal: names what was refused and the closed list.
pub(crate) fn refuse(tokens: impl ToTokens, what: impl std::fmt::Display) -> syn::Error {
    syn::Error::new_spanned(tokens, format!("{what}; {SUPPORTED}"))
}

/// [`refuse`] at a span rather than a token tree.
pub(crate) fn refuse_at(span: proc_macro2::Span, what: impl std::fmt::Display) -> syn::Error {
    syn::Error::new(span, format!("{what}; {SUPPORTED}"))
}

pub(crate) struct TypeAttrs {
    pub kind: Option<LitStr>,
    pub code: Option<(u32, Span)>,
    /// `index(...)` bodies, parsed against the kind's fields by `index::Indexes`.
    pub indexes: Vec<TokenStream>,
    /// `labels = L`: the enum is a machine's state, label `L` plus data shared by name across the
    /// labels (#3373 ruling 1).
    pub labels: Option<syn::Path>,
    /// `edges = E`: the code enum of a kind's edges, whose codes are the only `edge` codes its log
    /// entries may carry (#3392 A7).
    pub edges: Option<syn::Path>,
}

/// A field's storage attributes: at most one shape attribute, and an optional `alias = "old"`
/// naming the field's earlier name (#3392 A7).
pub(crate) struct FieldAttrs {
    pub shape: Option<FieldAttr>,
    pub alias: Option<LitStr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldAttr {
    Child,
    Records,
    History,
    Frozen,
    Cbor,
    Intern,
    Flatten,
    State,
}

impl FieldAttr {
    pub(crate) fn name(self) -> &'static str {
        match self {
            FieldAttr::Child => "child",
            FieldAttr::Records => "records",
            FieldAttr::History => "history",
            FieldAttr::Frozen => "frozen",
            FieldAttr::Cbor => "cbor",
            FieldAttr::Intern => "intern",
            FieldAttr::Flatten => "flatten",
            FieldAttr::State => "state",
        }
    }

    fn parse(name: &str) -> Option<FieldAttr> {
        [
            FieldAttr::Child,
            FieldAttr::Records,
            FieldAttr::History,
            FieldAttr::Frozen,
            FieldAttr::Cbor,
            FieldAttr::Intern,
            FieldAttr::Flatten,
            FieldAttr::State,
        ]
        .into_iter()
        .find(|attr| attr.name() == name)
    }
}

fn store_attrs(attrs: &[Attribute]) -> impl Iterator<Item = &Attribute> {
    attrs.iter().filter(|attr| attr.path().is_ident("store"))
}

fn path_name(path: &syn::Path) -> String {
    path.to_token_stream().to_string().replace(' ', "")
}

pub(crate) fn type_attrs(attrs: &[Attribute]) -> syn::Result<TypeAttrs> {
    let mut parsed = TypeAttrs { kind: None, code: None, indexes: Vec::new(), labels: None, edges: None };
    for attr in store_attrs(attrs) {
        attr.parse_nested_meta(|meta| {
            let name = path_name(&meta.path);
            match name.as_str() {
                "kind" => {
                    let value: LitStr = meta.value()?.parse()?;
                    if parsed.kind.replace(value).is_some() {
                        return Err(meta.error("`kind` is given twice"));
                    }
                }
                "code" => {
                    let value: LitInt = meta.value()?.parse()?;
                    let code = value.base10_parse::<u32>()?;
                    if parsed.code.replace((code, value.span())).is_some() {
                        return Err(meta.error("`code` is given twice"));
                    }
                }
                "index" => {
                    let content;
                    parenthesized!(content in meta.input);
                    parsed.indexes.push(content.parse()?);
                }
                "labels" => {
                    let value: syn::Path = meta.value()?.parse()?;
                    if parsed.labels.replace(value).is_some() {
                        return Err(meta.error("`labels` is given twice"));
                    }
                }
                "edges" => {
                    let value: syn::Path = meta.value()?.parse()?;
                    if parsed.edges.replace(value).is_some() {
                        return Err(meta.error("`edges` is given twice"));
                    }
                }
                _ if FieldAttr::parse(&name).is_some() || name == "alias" => {
                    return Err(refuse(&meta.path, format_args!("`{name}` is a field attribute, not a type attribute")));
                }
                _ => return Err(refuse(&meta.path, format_args!("unknown storage attribute `{name}`"))),
            }
            Ok(())
        })?;
    }
    Ok(parsed)
}

pub(crate) fn field_attrs(attrs: &[Attribute]) -> syn::Result<FieldAttrs> {
    let mut found = FieldAttrs { shape: None, alias: None };
    for attr in store_attrs(attrs) {
        attr.parse_nested_meta(|meta| {
            let name = path_name(&meta.path);
            if name == "alias" {
                let value: LitStr = meta.value()?.parse()?;
                if value.value().is_empty() {
                    return Err(refuse(&value, "`alias` names the field's earlier name, which is not empty"));
                }
                if found.alias.replace(value).is_some() {
                    return Err(meta.error("`alias` is given twice"));
                }
                return Ok(());
            }
            let Some(parsed) = FieldAttr::parse(&name) else {
                let what = match name.as_str() {
                    "kind" | "index" | "labels" | "edges" => format!("`{name}` is a type attribute, not a field attribute"),
                    "code" => "`code` belongs on enum variants and kinds, not fields".to_owned(),
                    _ => format!("unknown storage attribute `{name}`"),
                };
                return Err(refuse(&meta.path, what));
            };
            if !meta.input.is_empty() && !meta.input.peek(syn::Token![,]) {
                return Err(refuse(&meta.path, format_args!("`{name}` takes no arguments")));
            }
            if let Some(previous) = found.shape.replace(parsed) {
                return Err(refuse(
                    &meta.path,
                    format_args!("a field takes one storage attribute, found `{}` and `{name}`", previous.name()),
                ));
            }
            Ok(())
        })?;
    }
    Ok(found)
}

pub(crate) fn variant_code(attrs: &[Attribute]) -> syn::Result<Option<(i64, Span)>> {
    let mut code = None;
    for attr in store_attrs(attrs) {
        attr.parse_nested_meta(|meta| {
            let name = path_name(&meta.path);
            if name != "code" {
                return Err(refuse(&meta.path, format_args!("`{name}` is not a variant attribute")));
            }
            let value: LitInt = meta.value()?.parse()?;
            if code.replace((value.base10_parse::<i64>()?, value.span())).is_some() {
                return Err(meta.error("`code` is given twice"));
            }
            Ok(())
        })?;
    }
    Ok(code)
}
