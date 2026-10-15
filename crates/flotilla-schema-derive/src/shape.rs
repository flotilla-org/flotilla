//! Classifying a field: its attribute and its type together decide how it is stored.
//!
//! Classification refuses every shape outside the closed list. Code generation then matches on
//! [`FieldShape`]; later layers implement more shapes without changing classification.

use syn::{
    visit::{self, Visit},
    GenericArgument, Ident, PathArguments, Type,
};

use crate::attrs::{refuse, FieldAttr};

/// How one field is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldShape {
    /// Unattributed: the type's own `Column` implementation.
    Column,
    Intern {
        optional: bool,
    },
    Flatten {
        optional: bool,
    },
    State,
    Frozen {
        optional: bool,
    },
    Child(Collection),
    Records,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Collection {
    Vec,
    Map,
    Set,
}

/// Type constructors that need an attribute; a field of one of these without it is refused.
const NEEDS_ATTRIBUTE: &[&str] =
    &["Vec", "VecDeque", "LinkedList", "BinaryHeap", "BTreeMap", "BTreeSet", "HashMap", "HashSet", "Records", "Frozen", "State"];

pub(crate) fn classify(attr: Option<FieldAttr>, ty: &Type, owner: &Ident) -> syn::Result<FieldShape> {
    check_type(ty, owner)?;
    let (name, args) = head(ty).ok_or_else(|| refuse(ty, "this field type cannot be stored"))?;
    let shape = match attr {
        None => {
            let inner = if name == "Option" { args.first().and_then(|inner| head(inner)) } else { Some((name.clone(), args.clone())) };
            match inner {
                Some((inner, inner_args)) if is_blob(&inner, &inner_args) => FieldShape::Column,
                Some((inner, _)) if NEEDS_ATTRIBUTE.contains(&inner.as_str()) => {
                    return Err(refuse(ty, format_args!("a `{inner}` field needs a storage attribute")));
                }
                _ => FieldShape::Column,
            }
        }
        Some(FieldAttr::Child) => match name.as_str() {
            "Vec" => FieldShape::Child(Collection::Vec),
            "BTreeMap" => FieldShape::Child(Collection::Map),
            "BTreeSet" => FieldShape::Child(Collection::Set),
            _ => return Err(refuse(ty, "`child` needs a Vec, BTreeMap or BTreeSet field")),
        },
        Some(FieldAttr::Records | FieldAttr::History) if name == "Records" => {
            if attr == Some(FieldAttr::Records) {
                FieldShape::Records
            } else {
                FieldShape::History
            }
        }
        Some(attr @ (FieldAttr::Records | FieldAttr::History)) => {
            return Err(refuse(ty, format_args!("`{}` needs a Records<T> field", attr.name())));
        }
        Some(FieldAttr::Frozen) => match (name.as_str(), option_head(&name, &args).as_deref()) {
            ("Frozen", _) => FieldShape::Frozen { optional: false },
            (_, Some("Frozen")) => FieldShape::Frozen { optional: true },
            _ => return Err(refuse(ty, "`frozen` needs a Frozen<T> or Option<Frozen<T>> field")),
        },
        Some(FieldAttr::State) if name == "State" => FieldShape::State,
        Some(FieldAttr::State) => return Err(refuse(ty, "`state` needs a State<E> field")),
        Some(attr @ (FieldAttr::Intern | FieldAttr::Flatten)) => {
            let stored = option_head(&name, &args).unwrap_or_else(|| name.clone());
            if NEEDS_ATTRIBUTE.contains(&stored.as_str()) {
                return Err(refuse(ty, format_args!("`{}` cannot apply to a `{stored}` field", attr.name())));
            }
            if attr == FieldAttr::Intern {
                FieldShape::Intern { optional: name == "Option" }
            } else {
                FieldShape::Flatten { optional: name == "Option" }
            }
        }
    };
    Ok(shape)
}

/// The last path segment's name and its type arguments, through parentheses and groups.
fn head(ty: &Type) -> Option<(String, Vec<&Type>)> {
    match ty {
        Type::Group(group) => head(&group.elem),
        Type::Paren(paren) => head(&paren.elem),
        Type::Path(path) if path.qself.is_none() => {
            let segment = path.path.segments.last()?;
            let args = match &segment.arguments {
                PathArguments::AngleBracketed(args) => {
                    args.args.iter().filter_map(|arg| if let GenericArgument::Type(ty) = arg { Some(ty) } else { None }).collect()
                }
                _ => Vec::new(),
            };
            Some((segment.ident.to_string(), args))
        }
        _ => None,
    }
}

/// The first type argument of the last path segment: `T` of `Option<T>` or `State<T>`.
pub(crate) fn first_arg(ty: &Type) -> Option<&Type> {
    head(ty).and_then(|(_, args)| args.first().copied())
}

/// `K` and `V` of `BTreeMap<K, V>`.
pub(crate) fn map_args(ty: &Type) -> Option<(&Type, &Type)> {
    match head(ty)?.1.as_slice() {
        [key, value] => Some((key, value)),
        _ => None,
    }
}

/// For `Option<X<..>>`, the name `X`.
fn option_head(name: &str, args: &[&Type]) -> Option<String> {
    if name != "Option" {
        return None;
    }
    args.first().and_then(|inner| head(inner)).map(|(inner, _)| inner)
}

fn is_blob(name: &str, args: &[&Type]) -> bool {
    name == "Vec" && args.len() == 1 && head(args[0]).is_some_and(|(inner, inner_args)| inner == "u8" && inner_args.is_empty())
}

/// Refusals that hold whatever the attribute: recursion, `serde_json`, `Option<Option<_>>`, and
/// type forms with no storage (references, tuples, arrays, trait objects, ...).
fn check_type(ty: &Type, owner: &Ident) -> syn::Result<()> {
    let mut checker = TypeCheck { owner, error: None };
    checker.visit_type(ty);
    checker.error.map_or(Ok(()), Err)
}

struct TypeCheck<'a> {
    owner: &'a Ident,
    error: Option<syn::Error>,
}

impl TypeCheck<'_> {
    fn fail(&mut self, error: syn::Error) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }
}

impl<'ast> Visit<'ast> for TypeCheck<'_> {
    fn visit_type(&mut self, ty: &'ast Type) {
        match ty {
            Type::Path(_) | Type::Group(_) | Type::Paren(_) => {}
            _ => self.fail(refuse(ty, "this field type cannot be stored")),
        }
        if let Some(("Option", args)) = head(ty).as_ref().map(|(name, args)| (name.as_str(), args)) {
            if args.first().and_then(|inner| head(inner)).is_some_and(|(inner, _)| inner == "Option") {
                self.fail(refuse(ty, "`Option<Option<_>>` cannot be stored: absent and present-but-empty would share one null"));
            }
        }
        visit::visit_type(self, ty);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        for segment in &path.segments {
            if segment.ident == "Self" || segment.ident == *self.owner {
                self.fail(refuse(path, "a stored type cannot contain itself"));
            }
            if segment.ident == "serde_json" {
                self.fail(refuse(path, "`serde_json` values have no storage shape"));
            }
            // A reference names its target's kind without containing it, so `Ref<Self>` is not
            // recursion; its argument is only a marker.
            if segment.ident != "Ref" {
                visit::visit_path_arguments(self, &segment.arguments);
            }
        }
    }
}
