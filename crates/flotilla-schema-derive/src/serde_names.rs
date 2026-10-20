//! Serde's serialized enum names. Descriptions retain Rust field names, as Stored does;
//! rename_all_fields and variant-level rename_all only affect Serde's object field names.
use syn::{punctuated::Punctuated, Attribute, Expr, Lit, LitStr, Meta, Token};

/// Read a serialization-side name, consuming unrelated metadata (including lists) as syntax.
/// Deserialization names and aliases cannot change the values a descriptor advertises.
pub(crate) fn serialization_name(attrs: &[Attribute], key: &str) -> syn::Result<Option<LitStr>> {
    let mut result = None;
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
        for meta in attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)? {
            if !meta.path().is_ident(key) {
                continue;
            }
            match meta {
                Meta::NameValue(value) => result = Some(string(value.value)?),
                Meta::List(list) => {
                    for side in list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)? {
                        if let Meta::NameValue(value) = side {
                            if value.path.is_ident("serialize") {
                                result = Some(string(value.value)?);
                            }
                        }
                    }
                }
                other => return Err(syn::Error::new_spanned(other, "expected an enum serialization name")),
            }
        }
    }
    Ok(result)
}

fn string(value: Expr) -> syn::Result<LitStr> {
    match value {
        Expr::Group(value) => string(*value.expr),
        Expr::Paren(value) => string(*value.expr),
        Expr::Lit(value) => match value.lit {
            Lit::Str(value) => Ok(value),
            other => Err(syn::Error::new_spanned(other, "expected a string")),
        },
        other => Err(syn::Error::new_spanned(other, "expected a string")),
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) enum RenameRule {
    #[default]
    Pascal,
    Snake,
    Kebab,
    ScreamingSnake,
    ScreamingKebab,
    Lower,
    Upper,
    Camel,
}
impl RenameRule {
    pub(crate) fn parse(value: Option<LitStr>) -> syn::Result<Self> {
        Ok(match value.as_ref().map(LitStr::value).as_deref() {
            None | Some("PascalCase") => Self::Pascal,
            Some("snake_case") => Self::Snake,
            Some("kebab-case") => Self::Kebab,
            Some("SCREAMING_SNAKE_CASE") => Self::ScreamingSnake,
            Some("SCREAMING-KEBAB-CASE") => Self::ScreamingKebab,
            Some("lowercase") => Self::Lower,
            Some("UPPERCASE") => Self::Upper,
            Some("camelCase") => Self::Camel,
            Some(_) => return Err(syn::Error::new_spanned(value.unwrap(), "unsupported enum renaming")),
        })
    }
    pub(crate) fn apply(self, label: &str) -> String {
        match self {
            Self::Pascal => label.into(),
            Self::Snake => snake_case(label),
            Self::Kebab => snake_case(label).replace('_', "-"),
            Self::ScreamingSnake => snake_case(label).to_ascii_uppercase(),
            Self::ScreamingKebab => snake_case(label).to_ascii_uppercase().replace('_', "-"),
            Self::Lower => label.to_ascii_lowercase(),
            Self::Upper => label.to_ascii_uppercase(),
            Self::Camel => {
                let mut characters = label.chars();
                let mut name = characters.next().map(|c| c.to_ascii_lowercase().to_string()).unwrap_or_default();
                name.push_str(characters.as_str());
                name
            }
        }
    }
}

pub(crate) fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}
