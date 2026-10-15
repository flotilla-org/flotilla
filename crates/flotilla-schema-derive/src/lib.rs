//! `#[derive(Stored)]`: relational storage generated from Rust types (#3320).
//!
//! The derive accepts only the closed attribute list ruled on #3215 and refuses everything else
//! with a message naming that list. Generated code names only `::flotilla_schema` paths, so a
//! type deriving `Stored` compiles without any database backend.
//!
//! Modules: `attrs` parses `#[store(...)]`, `shape` classifies each field (attribute plus type)
//! into a `FieldShape`, `fields` generates one field's code through the adapter its attribute
//! selects, and `structs` and `enums` generate the trait implementations.
//!
//! Indirect recursion (`A` holds `B`, which holds `A`) is not visible to a derive; the compiler
//! refuses it: by value it is an infinitely sized type (E0072), and through tables (`A` holds a
//! `Vec<B>` whose rows hold a `Vec<A>`) proving that the rows are elements overflows (E0275).
//! `Ref<K>` names a kind without containing it, so references are not recursion.

mod attrs;
mod enums;
mod fields;
mod shape;
mod structs;

use proc_macro::TokenStream;
use syn::{Data, DeriveInput, Fields, GenericParam};

use crate::attrs::refuse;

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
        Data::Enum(data) => enums::expand(input, data, type_attrs),
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) if fields.named.is_empty() => Err(structs::refuse_empty(input)),
            Fields::Named(fields) => structs::expand(input, fields, type_attrs),
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => structs::newtype(input, fields, type_attrs),
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
            quote! { enum A { #[store(code = 1)] B, C } },
            quote! { enum A { #[store(code = 1)] B, #[store(code = 1)] C } },
            quote! { enum A { #[store(code = 1)] Ab { c: String }, #[store(code = 2)] AB { c: String } } },
            quote! { enum A { #[store(code = 1)] B { a: String }, #[store(code = 1)] C } },
            quote! { enum A { #[store(code = 1)] B { a: String }, C { b: i64 } } },
            quote! { enum A {} },
            quote! { #[store(kind = "A", code = 1)] enum A { #[store(code = 1)] B } },
            quote! { enum A { #[store(code = 1)] B { #[store(state)] a: String } } },
            quote! { enum A { #[store(code = 1)] B { a: Option<A> } } },
            quote! { struct A { #[store(child)] a: Option<Vec<String>> } },
            quote! { struct A { #[store(child)] a: Vec<Self> } },
            quote! { struct A { #[store(records)] a: Records<A> } },
            quote! { struct A { #[store(frozen)] a: Option<Option<Frozen<Doc>>> } },
            quote! { struct A(#[store(intern)] String); },
            quote! { struct A(Vec<String>); },
            quote! { struct A(Option<Box<A>>); },
        ];
        for case in cases {
            let message = error(case.clone());
            assert!(message.contains(SUPPORTED), "{case} was refused without naming the list: {message}");
        }
    }

    // Shapes on the list that a later layer implements are refused plainly, not as outside the list.
    #[test]
    fn listed_shapes_not_yet_implemented_say_so() {
        let message = error(quote! { #[store(kind = "A", code = 1, index(name = "x", eq(a)))] struct A { a: String } });
        assert!(message.contains("is not supported yet") && !message.contains(SUPPORTED), "{message}");
    }

    // Glue: each table and frozen attribute selects its adapter and bounds the element, key or
    // content type by the trait it needs; a part holding a table is a row only, never `Flat`.
    #[test]
    fn tables_and_frozen_select_adapters_and_bounds() {
        let part = expanded(quote! {
            struct Part<I> {
                #[store(child)] a: Vec<Item>,
                #[store(child)] b: BTreeMap<String, Value>,
                #[store(child)] c: BTreeSet<Tag>,
                #[store(records)] d: Records<Event<I>>,
                #[store(history)] e: Records<Attempt>,
                #[store(frozen)] f: Frozen<Doc>,
                #[store(frozen)] g: Option<Frozen<Plan>>,
            }
        });
        for expected in [
            "Item : :: flotilla_schema :: Element",
            "String : :: flotilla_schema :: KeyColumn + :: core :: cmp :: Ord , Value : :: flotilla_schema :: Element",
            "Tag : :: flotilla_schema :: KeyColumn",
            "Event < I > : :: flotilla_schema :: Element",
            "Doc : :: flotilla_schema :: Freezable",
            "Plan : :: flotilla_schema :: Freezable",
            "adapt :: AsChild as :: flotilla_schema :: adapt :: Adapter < BTreeSet < Tag > >",
            "adapt :: AsRecords as :: flotilla_schema :: adapt :: Adapter < Records < Event < I > > >",
            "adapt :: AsHistory as :: flotilla_schema :: adapt :: Adapter < Records < Attempt > >",
            "adapt :: AsOptionalFrozen as :: flotilla_schema :: adapt :: Adapter < Option < Frozen < Plan > > >",
        ] {
            assert!(part.contains(expected), "missing {expected}");
        }
        assert!(!part.contains("Flat for"), "a part with tables cannot be flattened");
        let flat = expanded(quote! { struct Leaf { #[store(frozen)] f: Frozen<Doc> } });
        assert!(flat.contains(":: flotilla_schema :: Flat for Leaf"), "a frozen hash is a column, so the part stays flat");
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

    fn expanded(input: proc_macro2::TokenStream) -> String {
        let input: DeriveInput = syn::parse2(input).expect("parse input");
        expand(&input).expect("expands").to_string()
    }

    // Glue: each attribute selects its adapter and the bound naming the trait it needs, so a
    // field type outside the list is reported at that trait's diagnostic.
    #[test]
    fn attributes_select_adapters_and_bounds() {
        let expanded = expanded(quote! {
            struct Part {
                #[store(intern)] a: String,
                #[store(intern)] b: Option<Name>,
                #[store(flatten)] c: Leaf,
                #[store(flatten)] d: Option<Leaf>,
                #[store(state)] e: State<Phase>,
                f: Ref<Inbox>,
                g: Option<Ref<Part>>,
            }
        });
        for expected in [
            "String : :: flotilla_schema :: Intern",
            "Name : :: flotilla_schema :: Intern",
            "Leaf : :: flotilla_schema :: Flat",
            "Phase : :: flotilla_schema :: CodeEnum",
            "Ref < Inbox > : :: flotilla_schema :: Column",
            "adapt :: AsOptionalInterned as :: flotilla_schema :: adapt :: Adapter < Option < Name > >",
            "adapt :: AsOptionalFlat as :: flotilla_schema :: adapt :: Adapter < Option < Leaf > >",
            "adapt :: AsInterned as :: flotilla_schema :: adapt :: Adapter < String >",
        ] {
            assert!(expanded.contains(expected), "missing {expected}");
        }
    }

    // Behaviour (DESIGN): a kind's `Kind` impl carries only bounds of fields that mention a type
    // parameter, so a self-reference (`Ref<Message>`) does not make proving `Message: Kind` cyclic.
    #[test]
    fn kind_impls_bound_only_generic_fields() {
        let expanded = expanded(quote! {
            #[store(kind = "Message", code = 1)]
            struct Message<T> { reply: Option<Ref<Message<T>>>, body: String, extra: T }
        });
        let kind = &expanded[expanded.find(":: flotilla_schema :: Kind for").expect("kind impl")..];
        let header = &kind[..kind.find('{').expect("body")];
        assert!(header.contains("T : :: flotilla_schema :: Column"));
        assert!(header.contains("Option < Ref < Message < T > > > : :: flotilla_schema :: Column"), "mentions T");
        assert!(!header.contains("String :"), "concrete fields are checked through Element only: {header}");
    }

    // Behaviour (DESIGN codes): a code enum's CODES are ascending whatever the declaration order,
    // and a data enum's variant columns are `<variant>_<field>` below the prefix.
    #[test]
    fn enums_take_codes_and_variant_prefixes() {
        let code = expanded(quote! { enum Phase { #[store(code = 3)] Late, #[store(code = 1)] Early } });
        assert!(code.contains("const CODES : & 'static [i64] = & [1i64 , 3i64]"), "{code}");
        let data = expanded(quote! { enum Edge { #[store(code = 2)] DropIt { #[store(intern)] by: String }, #[store(code = 1)] Go } });
        assert!(data.contains("\"drop_it_by\""), "{data}");
        assert!(data.contains("layout . guard (tag , 2i64"), "{data}");
        assert!(!data.contains("layout . guard (tag , 1i64"), "unit variants have no guard");
    }

    // Behaviour: a newtype wrapping String is internable; other newtypes are not.
    #[test]
    fn newtypes_of_string_intern() {
        assert!(expanded(quote! { struct Name(String); }).contains(":: flotilla_schema :: Intern for Name"));
        assert!(!expanded(quote! { struct Count(u32); }).contains("Intern for"));
    }
}
