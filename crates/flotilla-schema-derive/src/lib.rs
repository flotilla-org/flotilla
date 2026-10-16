//! `#[derive(Stored)]`: relational storage generated from Rust types (#3320).
//!
//! The derive accepts only the closed attribute list ruled on #3215 and refuses everything else
//! with a message naming that list. Generated code names only `::flotilla_schema` paths, so a
//! type deriving `Stored` compiles without any database backend.
//!
//! Modules: `attrs` parses `#[store(...)]`, `shape` classifies each field (attribute plus type)
//! into a `FieldShape`, and `structs` generates the trait implementations.
//!
//! Indirect recursion (`A` holds `B`, which holds `A`) is not visible to a derive; it is refused by
//! the compiler as a cycle evaluating the `TABLES` constants (E0391).

mod attrs;
mod shape;
mod structs;

use proc_macro::TokenStream;
use syn::{Data, DeriveInput, Fields, GenericParam};

use crate::attrs::{not_yet, refuse};

#[proc_macro_derive(Stored, attributes(store))]
pub fn derive_stored(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    expand(&input).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// The derive's expansion, separate from the `proc_macro` entry point so tests can call it.
pub(crate) fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let type_attrs = attrs::type_attrs(&input.attrs)?;
    if let Some(lifetime) = input.generics.params.iter().find(|param| matches!(param, GenericParam::Lifetime(_))) {
        return Err(refuse(lifetime, "a stored type owns its data and takes no lifetime parameters"));
    }
    match &input.data {
        Data::Union(data) => Err(refuse(data.union_token, "unions cannot be stored")),
        Data::Enum(data) => {
            // Parse every variant's attributes and fields now, so refusals of shapes outside the
            // list are reported before the not-yet-supported error.
            for variant in &data.variants {
                attrs::variant_code(&variant.attrs)?;
                match &variant.fields {
                    Fields::Unnamed(fields) => return Err(refuse(fields, "tuple variants cannot be stored; use named fields")),
                    Fields::Named(fields) => {
                        for field in &fields.named {
                            shape::classify(attrs::field_attr(&field.attrs)?, &field.ty, &input.ident)?;
                        }
                    }
                    Fields::Unit => {}
                }
            }
            Err(not_yet(&input.ident, "deriving `Stored` for an enum"))
        }
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) if fields.named.is_empty() => Err(structs::refuse_empty(input)),
            Fields::Named(fields) => structs::expand(input, fields, type_attrs),
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                let field = &fields.unnamed[0];
                shape::classify(attrs::field_attr(&field.attrs)?, &field.ty, &input.ident)?;
                Err(not_yet(&input.ident, "a newtype struct"))
            }
            Fields::Unnamed(fields) => Err(refuse(fields, "tuple structs with more than one field cannot be stored; use named fields")),
            Fields::Unit => Err(structs::refuse_empty(input)),
        },
    }
}

#[cfg(test)]
mod tests {
    use quote::quote;
    use syn::DeriveInput;

    use super::expand;
    use crate::attrs::SUPPORTED;

    fn error(input: proc_macro2::TokenStream) -> String {
        let input: DeriveInput = syn::parse2(input).expect("parse input");
        expand(&input).expect_err("refused").to_string()
    }

    // Behaviour (#3320): every shape outside the closed list fails to compile with a message
    // naming the list. The cases cover each refusal family: unknown or misplaced attributes,
    // attributes on the wrong type, collections without an attribute, recursion, serde_json,
    // Option<Option<_>>, unions, multi-field tuple structs, tuple variants and non-path types.
    #[test]
    fn refusals_name_the_closed_list() {
        let cases = [
            quote! { struct A { #[store(unknown)] a: String } },
            quote! { struct A { #[store(index)] a: String } },
            quote! { struct A { #[store(code = 1)] a: String } },
            quote! { struct A { #[store(child, intern)] a: Vec<String> } },
            quote! { struct A { #[store(intern(x))] a: String } },
            quote! { #[store(child)] struct A { a: String } },
            quote! { #[store(rename = "x")] struct A { a: String } },
            quote! { struct A { #[store(child)] a: String } },
            quote! { struct A { #[store(records)] a: Vec<String> } },
            quote! { struct A { #[store(history)] a: Vec<String> } },
            quote! { struct A { #[store(frozen)] a: String } },
            quote! { struct A { #[store(state)] a: String } },
            quote! { struct A { #[store(intern)] a: Vec<String> } },
            quote! { struct A { #[store(flatten)] a: BTreeMap<String, String> } },
            quote! { struct A { a: Vec<String> } },
            quote! { struct A { a: Option<BTreeSet<String>> } },
            quote! { struct A { a: Records<Event> } },
            quote! { struct A { a: Frozen<Doc> } },
            quote! { struct A { a: State<Phase> } },
            quote! { struct A { a: HashMap<String, String> } },
            quote! { struct A { a: Option<A> } },
            quote! { struct A { a: Option<Box<Self>> } },
            quote! { struct A { a: serde_json::Value } },
            quote! { struct A { a: Option<::serde_json::Map<String, String>> } },
            quote! { struct A { a: Option<Option<String>> } },
            quote! { struct A { a: Option<core::option::Option<String>> } },
            quote! { struct A { a: (String, String) } },
            quote! { struct A { a: [u8; 32] } },
            quote! { struct A { a: &'static str } },
            quote! { struct A<'a> { a: std::borrow::Cow<'a, str> } },
            quote! { union A { a: u32 } },
            quote! { struct A(String, String); },
            quote! { struct A; },
            quote! { struct A {} },
            quote! { enum A { #[store(code = 1)] B(String) } },
            quote! { enum A { #[store(child)] B } },
            quote! { enum A { #[store(code = 1)] B { a: Vec<String> } } },
        ];
        for case in cases {
            let message = error(case.clone());
            assert!(message.contains(SUPPORTED), "{case} was refused without naming the list: {message}");
        }
    }

    // Shapes on the list that later layers implement are refused plainly, not as outside the list.
    #[test]
    fn listed_shapes_not_yet_implemented_say_so() {
        let cases = [
            quote! { struct A { #[store(intern)] a: String } },
            quote! { struct A { #[store(flatten)] a: Option<Part> } },
            quote! { struct A { #[store(state)] a: State<Phase> } },
            quote! { struct A { #[store(frozen)] a: Option<Frozen<Doc>> } },
            quote! { struct A { #[store(child)] a: BTreeMap<String, String> } },
            quote! { struct A { #[store(records)] a: Records<Event> } },
            quote! { struct A { #[store(history)] a: Records<Event> } },
            quote! { #[store(kind = "A", code = 1, index(name = "x", eq(a)))] struct A { a: String } },
            quote! { struct A(String); },
            quote! { enum A { #[store(code = 1)] B, #[store(code = 2)] C { a: String } } },
        ];
        for case in cases {
            let message = error(case.clone());
            assert!(message.contains("is not supported yet") && !message.contains(SUPPORTED), "{case}: {message}");
        }
    }

    #[test]
    fn kinds_need_a_name_and_a_code_together() {
        assert!(error(quote! { #[store(kind = "A")] struct A { a: String } }).contains("needs a stable `code = N`"));
        assert!(error(quote! { #[store(code = 1)] struct A { a: String } }).contains("needs `kind"));
        assert!(error(quote! { #[store(kind = "snake_case", code = 1)] struct A { a: String } }).contains("UpperCamelCase"));
        assert!(error(quote! { #[store(kind = "A", kind = "B", code = 1)] struct A { a: String } }).contains("twice"));
    }

    // Glue: an accepted kind expands to the Element and Kind impls with per-field Column bounds.
    #[test]
    fn kinds_expand_with_per_field_bounds() {
        let input: DeriveInput = syn::parse2(quote! {
            #[store(kind = "Message", code = 7)]
            struct Message<T> { body: String, r#type: Option<T> }
        })
        .expect("parse");
        let expanded = expand(&input).expect("expands").to_string();
        assert!(expanded.contains("impl < T > :: flotilla_schema :: Kind for Message < T >"));
        assert!(expanded.contains("String : :: flotilla_schema :: Column"));
        assert!(expanded.contains("Option < T > : :: flotilla_schema :: Column"));
        assert!(expanded.contains("\"type\""), "raw identifiers store under their plain name");
    }
}
