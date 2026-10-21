//! Parsing `#[store(...)]`: the closed attribute list, and nothing else.

use proc_macro2::{Span, TokenStream};
use quote::ToTokens;
use syn::{parenthesized, Attribute, LitStr};

/// The closed attribute list, named by every refusal. `flotilla-schema` repeats this text in its
/// trait diagnostics and a test there keeps the two equal.
pub(crate) const SUPPORTED: &str = "the storage derive supports only the attributes child, records, history, frozen, cbor, intern, \
                                    flatten, state, alias and code (fields and variants) and kind, code, state, index, labels and edges (types)";

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
    pub code: Option<(String, Span)>,
    /// `index(...)` bodies, parsed against the kind's fields by `index::Indexes`.
    pub indexes: Vec<TokenStream>,
    /// `labels = L`: the enum is a machine's state, label `L` plus data shared by name across the
    /// labels (#3373 ruling 1).
    pub labels: Option<syn::Path>,
    /// `state = S` on a kind's spec: the kind is stored through a generic holder (the engine's
    /// `Resource<M>`), which lays out the spec, then its state `S` as the field `state`, then
    /// its records (#3392 ruling D5).
    pub state: Option<syn::Type>,
    /// `edges = E`: the code enum of a kind's edges, whose codes are the only `edge` codes its log
    /// entries may carry (#3392 A7).
    pub edges: Option<syn::Path>,
    /// `intern = "vc"` on a newtype of `String`: the type is interned text in that domain wherever
    /// it is stored, a map key or set value included, as an `Address` is in `ad`.
    pub intern: Option<LitStr>,
}

/// A field's storage attributes: at most one shape attribute, and an optional `alias = "old"`
/// naming the field's earlier name (#3392 A7).
pub(crate) struct FieldAttrs {
    pub shape: Option<FieldAttr>,
    pub alias: Option<LitStr>,
    pub domain: Option<LitStr>,
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

fn shape_attrs(attrs: &[Attribute]) -> impl Iterator<Item = &Attribute> {
    attrs.iter().filter(|attr| attr.path().is_ident("shape"))
}

pub(crate) enum ShapeAttr {
    Child,
    Flatten,
    Intern(LitStr),
    Code(LitStr),
}

/// Parse the shape namespace once. Storage policy never reaches the storage parser through it.
pub(crate) fn validate_shape_attr(attr: &Attribute) -> syn::Result<Vec<ShapeAttr>> {
    let mut parsed = Vec::new();
    attr.parse_nested_meta(|meta| {
        let value = if meta.path.is_ident("code") {
            ShapeAttr::Code(meta.value()?.parse()?)
        } else if meta.path.is_ident("intern") {
            ShapeAttr::Intern(meta.value()?.parse()?)
        } else if meta.path.is_ident("child") || meta.path.is_ident("flatten") {
            if !meta.input.is_empty() && !meta.input.peek(syn::Token![,]) {
                return Err(meta.error("child and flatten take no arguments"));
            }
            if meta.path.is_ident("child") {
                ShapeAttr::Child
            } else {
                ShapeAttr::Flatten
            }
        } else {
            return Err(meta.error("shape attributes are child, intern, flatten and code; storage attributes belong in #[store(...)]"));
        };
        parsed.push(value);
        Ok(())
    })?;
    Ok(parsed)
}

fn path_name(path: &syn::Path) -> String {
    path.to_token_stream().to_string().replace(' ', "")
}

pub(crate) fn type_attrs(attrs: &[Attribute]) -> syn::Result<TypeAttrs> {
    let mut parsed = TypeAttrs { kind: None, code: None, indexes: Vec::new(), labels: None, state: None, edges: None, intern: None };
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
                    let value = code_literal(meta.value()?.parse()?)?;
                    let code = code(&value)?;
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
                "state" if meta.input.peek(syn::Token![=]) => {
                    let value: syn::Type = meta.value()?.parse()?;
                    if parsed.state.replace(value).is_some() {
                        return Err(meta.error("`state` is given twice"));
                    }
                }
                "edges" => {
                    let value: syn::Path = meta.value()?.parse()?;
                    if parsed.edges.replace(value).is_some() {
                        return Err(meta.error("`edges` is given twice"));
                    }
                }
                "intern" if meta.input.peek(syn::Token![=]) => {
                    let value: LitStr = meta.value()?.parse()?;
                    code(&value)?;
                    if parsed.intern.replace(value).is_some() {
                        return Err(meta.error("`intern` is given twice"));
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
    for attr in shape_attrs(attrs) {
        for shape in validate_shape_attr(attr)? {
            let ShapeAttr::Code(value) = shape else {
                return Err(refuse(attr, "only code is a type shape attribute"));
            };
            let value = (code(&value)?, value.span());
            if parsed.code.replace(value).is_some() {
                return Err(syn::Error::new_spanned(attr, "`code` is given twice"));
            }
        }
    }
    Ok(parsed)
}

pub(crate) fn field_attrs(attrs: &[Attribute]) -> syn::Result<FieldAttrs> {
    let mut found = FieldAttrs { shape: None, alias: None, domain: None };
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
            if parsed == FieldAttr::Intern {
                let value: LitStr = meta
                    .value()
                    .and_then(|input| input.parse())
                    .map_err(|_| refuse(&meta.path, "`intern` requires a domain, such as `intern = \"ad\"`"))?;
                code(&value)?;
                found.domain = Some(value);
            } else if !meta.input.is_empty() && !meta.input.peek(syn::Token![,]) {
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
    for attr in shape_attrs(attrs) {
        for shape in validate_shape_attr(attr)? {
            let parsed = match shape {
                ShapeAttr::Child => FieldAttr::Child,
                ShapeAttr::Flatten => FieldAttr::Flatten,
                ShapeAttr::Intern(domain) => {
                    code(&domain)?;
                    found.domain = Some(domain);
                    FieldAttr::Intern
                }
                ShapeAttr::Code(_) => return Err(refuse(attr, "code belongs on enum variants and kinds, not fields")),
            };
            if let Some(previous) = found.shape.replace(parsed) {
                return Err(refuse(
                    attr,
                    format_args!("a field takes one shape or storage attribute, found `{}` and `{}`", previous.name(), parsed.name()),
                ));
            }
        }
    }
    Ok(found)
}

pub(crate) fn variant_code(attrs: &[Attribute]) -> syn::Result<Option<(String, Span)>> {
    let mut code = None;
    for attr in store_attrs(attrs) {
        attr.parse_nested_meta(|meta| {
            let name = path_name(&meta.path);
            if name != "code" {
                return Err(refuse(&meta.path, format_args!("`{name}` is not a variant attribute")));
            }
            let value = code_literal(meta.value()?.parse()?)?;
            if code.replace((self::code(&value)?, value.span())).is_some() {
                return Err(meta.error("`code` is given twice"));
            }
            Ok(())
        })?;
    }
    for attr in shape_attrs(attrs) {
        for shape in validate_shape_attr(attr)? {
            let ShapeAttr::Code(value) = shape else {
                return Err(refuse(attr, "only code is a variant shape attribute"));
            };
            if code.replace((self::code(&value)?, value.span())).is_some() {
                return Err(syn::Error::new_spanned(attr, "`code` is given twice"));
            }
        }
    }
    Ok(code)
}

/// The longest code, in bytes; `flotilla_schema::Code::MAX` repeats it, and its `const fn new`
/// checks every generated code again when the crate compiles.
const CODE_MAX: usize = 8;

/// The literal of `code = ..`: text, refusing the integer codes #3392 ruling G7 retired.
fn code_literal(value: syn::Lit) -> syn::Result<LitStr> {
    match value {
        syn::Lit::Str(text) => Ok(text),
        other => Err(syn::Error::new_spanned(other, "a code is short text written by hand, such as `code = \"dr\"`, not a number")),
    }
}

/// A stored code (#3392 ruling G7): short explicit text, one to eight bytes, a lowercase ASCII
/// letter then lowercase letters, digits or `_`. Written by hand, never generated from a name.
fn code(value: &LitStr) -> syn::Result<String> {
    let code = value.value();
    let mut bytes = code.bytes();
    let valid = code.len() <= CODE_MAX
        && bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    if valid {
        Ok(code)
    } else {
        Err(syn::Error::new_spanned(
            value,
            format!("a code is short text written by hand: one to {CODE_MAX} bytes, a lowercase ASCII letter then lowercase letters, digits or `_`, such as \"dr\""),
        ))
    }
}

#[cfg(test)]
mod shape_tests {
    use quote::quote;
    use syn::DeriveInput;

    // Review #3592: separate namespaces cannot bypass storage-policy refusals or make mixed
    // duplicate attributes depend on source ordering. Enumerate all storage-only field policies.
    #[test]
    fn shape_namespaces_are_validated_independently() {
        for storage in ["records", "history", "frozen", "cbor", "state"] {
            let name = syn::Ident::new(storage, proc_macro2::Span::call_site());
            let input: DeriveInput = syn::parse2(quote!(
                struct Row {
                    #[shape(#name)]
                    value: String,
                }
            ))
            .unwrap();
            assert!(crate::expand(&input).is_err());
        }
        let mut messages = Vec::new();
        for input in [
            quote!(
                #[shape(code = "a")]
                #[store(code = "b")]
                struct Row {
                    value: String,
                }
            ),
            quote!(
                #[store(code = "b")]
                #[shape(code = "a")]
                struct Row {
                    value: String,
                }
            ),
        ] {
            let input: DeriveInput = syn::parse2(input).unwrap();
            messages.push(crate::expand(&input).unwrap_err().to_string());
        }
        assert_eq!(messages, vec!["`code` is given twice"; 2]);
    }
}
