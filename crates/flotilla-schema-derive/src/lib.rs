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
mod describe;
mod enums;
mod fields;
mod index;
mod shape;
mod structs;

use proc_macro::TokenStream;
use syn::{Data, DeriveInput, Fields, GenericParam};

use crate::attrs::refuse;

#[proc_macro_derive(Stored, attributes(store, shape))]
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
            // A kind's spec may be empty: the holder stores the state and records.
            Fields::Named(fields) if fields.named.is_empty() && type_attrs.state.is_none() => Err(structs::refuse_empty(input)),
            Fields::Named(fields) => structs::expand(input, fields, type_attrs),
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => structs::newtype(input, fields, type_attrs),
            Fields::Unnamed(fields) => Err(refuse(fields, "tuple structs with more than one field cannot be stored; use named fields")),
            Fields::Unit => Err(structs::refuse_empty(input)),
        },
    }
}

#[proc_macro_derive(Describe, attributes(shape))]
pub fn derive_describe(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    describe::expand(&input).unwrap_or_else(syn::Error::into_compile_error).into()
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
            quote! { struct A { #[store(code = "c1")] a: String } },
            quote! { struct A { #[store(child, intern)] a: Vec<String> } },
            quote! { struct A { #[store(intern(x))] a: String } },
            quote! { struct A { #[store(cbor, frozen)] a: String } },
            quote! { #[store(child)] struct A { a: String } },
            quote! { #[store(rename = "x")] struct A { a: String } },
            quote! { struct A { #[store(child)] a: String } },
            quote! { struct A { #[store(records)] a: Vec<String> } },
            quote! { struct A { #[store(history)] a: Vec<String> } },
            quote! { struct A { #[store(frozen)] a: String } },
            quote! { struct A { #[store(state)] a: String } },
            quote! { struct A { #[store(intern = "tx")] a: Vec<String> } },
            quote! { struct A { #[store(flatten)] a: BTreeMap<String, String> } },
            quote! { struct A { a: Vec<String> } },
            quote! { struct A { a: std::collections::BTreeMap<String, String> } },
            quote! { struct A { a: flotilla_schema::Records<String> } },
            quote! { struct A { a: flotilla_schema::Frozen<String> } },
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
            quote! { enum A { #[store(code = "c1")] B(String) } },
            quote! { enum A { #[store(child)] B } },
            quote! { enum A { #[store(code = "c1")] B { a: Vec<String> } } },
            quote! { enum A { #[store(code = "c1")] B, C } },
            quote! { enum A { #[store(code = "c1")] B, #[store(code = "c1")] C } },
            quote! { enum A { #[store(code = "c1")] Ab { c: String }, #[store(code = "c2")] AB { c: u32 } } },
            quote! { enum A { #[store(code = "c1")] Ab { c: String }, #[store(code = "c2")] AB { c: Option<u32> } } },
            quote! { enum A { #[store(code = "c1")] Ab { #[store(intern = "tx")] c: String }, #[store(code = "c2")] AB { c: Option<String> } } },
            quote! { enum A { #[store(code = "c1")] B { a: String }, #[store(code = "c1")] C } },
            quote! { enum A { #[store(code = "c1")] B { a: String }, C { b: i64 } } },
            quote! { enum A {} },
            quote! { #[store(kind = "A", code = "k1")] enum A { #[store(code = "c1")] B } },
            quote! { enum A { #[store(code = "c1")] B { #[store(state)] a: String } } },
            quote! { enum A { #[store(code = "c1")] B { a: Option<A> } } },
            quote! { struct A { #[store(child)] a: Option<Vec<String>> } },
            quote! { struct A { #[store(child)] a: Vec<Self> } },
            quote! { struct A { #[store(records)] a: Records<A> } },
            quote! { struct A { #[store(frozen)] a: Option<Option<Frozen<Doc>>> } },
            quote! { struct A(#[store(intern = "tx")] String); },
            quote! { struct A(Vec<String>); },
            quote! { struct A(Option<Box<A>>); },
        ];
        for case in cases {
            let message = error(case.clone());
            assert!(message.contains(SUPPORTED), "{case} was refused without naming the list: {message}");
        }
    }

    // Behaviour (#3392 G7): a code is short text written by hand, one to eight bytes, a lowercase
    // letter then lowercase letters, digits or `_`; an integer code, anything else, or a code two
    // variants share is refused at compile time.
    #[test]
    fn codes_are_short_hand_written_text() {
        for case in [
            quote! { enum A { #[store(code = 1)] B } },
            quote! { enum A { #[store(code = "")] B } },
            quote! { enum A { #[store(code = "Dr")] B } },
            quote! { enum A { #[store(code = "1d")] B } },
            quote! { enum A { #[store(code = "toolong_")] B, #[store(code = "abcdefghi")] C } },
            quote! { enum A { #[store(code = "a-b")] B } },
            quote! { #[store(kind = "A", code = 1)] struct A { a: String } },
            quote! { #[store(kind = "A", code = "A")] struct A { a: String } },
        ] {
            let message = error(case.clone());
            assert!(message.contains("a code is short text written by hand"), "{case}: {message}");
        }
        assert!(error(quote! { enum A { #[store(code = "dr")] B, #[store(code = "dr")] C } })
            .contains("code \"dr\" is used by both `B` and `C`"));
        assert!(expand(&syn::parse2(quote! { enum A { #[store(code = "abcdefgh")] B, #[store(code = "x_9")] C } }).expect("parse")).is_ok());
    }

    // Behaviour (#3392 G5, G6): a datum shared by name across a data enum's or a state's variants
    // has one type and attribute in each, or is `T` in some and `Option<T>` in others; the derive
    // lays it out once, as the option, and requires it under the variants carrying `T`.
    #[test]
    fn a_datum_may_be_t_in_one_variant_and_an_option_in_another() {
        for case in [
            quote! { enum A { #[store(code = "b")] B { #[store(intern = "tx")] a: String }, #[store(code = "c")] C { #[store(intern = "tx")] a: Option<String> } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "b")] B { #[store(flatten)] a: P }, #[store(code = "c")] C { #[store(flatten)] a: Option<P> } } },
        ] {
            let expanded = expanded(case.clone());
            assert!(expanded.contains("Layout :: required_of"), "{case}: {expanded}");
            assert!(expanded.contains("(0usize , true)") && expanded.contains("(0usize , false)"), "{case}: {expanded}");
        }
        let uniform = expanded(quote! { enum A { #[store(code = "b")] B { a: String }, #[store(code = "c")] C { a: String } } });
        assert!(!uniform.contains("required_of"), "{uniform}");
    }

    // Behaviour (#3373 ruling 1): a label-plus-data state is refused, naming the closed list, when
    // a datum appears with different types or attributes in two variants, when a variant has no
    // code (or two share one), when a datum takes a storage form other than plain, `child` (a map
    // or set), `intern` or `flatten`, and when `labels` is put anywhere but on an enum.
    #[test]
    fn label_state_refusals_name_the_closed_list() {
        let cases = [
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { a: String }, #[store(code = "c2")] C { a: u32 } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { a: String }, #[store(code = "c2")] C { #[store(intern = "tx")] a: String } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { #[store(flatten)] a: P }, #[store(code = "c2")] C { #[store(flatten)] a: Option<Q> } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { #[store(flatten)] a: P }, #[store(code = "c2")] C { a: P } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { #[store(child)] a: BTreeSet<String> }, #[store(code = "c2")] C { #[store(child)] a: BTreeMap<String, String> } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B, C } },
            quote! { #[store(labels = L)] enum A { B { a: String }, #[store(code = "c2")] C } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B, #[store(code = "c1")] C } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B(String) } },
            quote! { #[store(labels = L)] enum A {} },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { #[store(frozen)] a: Frozen<Doc> } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { #[store(cbor)] a: Doc } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { #[store(child)] a: Vec<String> } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { #[store(records)] a: Records<String> } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { #[store(state)] a: State<P> } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { a: Vec<String> } } },
            quote! { #[store(labels = L, index(name = "x", eq(a)))] enum A { #[store(code = "c1")] B } },
            quote! { #[store(labels = L)] struct A { a: String } },
            quote! { #[store(labels = L)] struct A(String); },
            quote! { struct A { #[store(labels)] a: String } },
            quote! { enum A { #[store(labels = L)] B } },
        ];
        for case in cases {
            let message = error(case.clone());
            assert!(message.contains(SUPPORTED), "{case} was refused without naming the list: {message}");
        }
        let mismatch =
            error(quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { a: String }, #[store(code = "c2")] C { a: u32 } } });
        assert!(mismatch.contains("`a` is declared as `String` in `B` and as `u32` in `C`"), "{mismatch}");
        assert!(mismatch.contains("or is `T` in some and `Option<T>` in others"), "{mismatch}");
        assert!(error(quote! { #[store(labels = L, labels = M)] enum A { #[store(code = "c1")] B } }).contains("twice"));
        // #3422: interned data of different Rust types can share a column when its domain agrees.
        for case in [
            quote! { enum A { #[store(code = "c1")] B { #[store(intern = "tx")] a: String }, #[store(code = "c2")] C { #[store(intern = "tx")] a: Name } } },
            quote! { #[store(labels = L)] enum A { #[store(code = "c1")] B { #[store(intern = "tx")] a: String }, #[store(code = "c2")] C { #[store(intern = "tx")] a: Name } } },
        ] {
            let _ = expanded(case);
        }
    }

    // Glue (#3373 ruling 1): a label-plus-data state is `Labelled` by its labels enum and
    // `StateData` with the variants' codes ascending and its data named once each, in order of
    // first declaration; its layout lays out each datum once, unprefixed, emits one label guard per
    // variant naming the datums it carries, and checks the labels enum agrees code for code; each
    // datum's delta is row by row for a collection and by columns otherwise. It is not a column or
    // a row: it is stored only as a `State`.
    #[test]
    fn label_states_expand_to_labelled_state_data() {
        let state = expanded(quote! {
            #[store(labels = TicketLabel)]
            enum TicketState {
                #[store(code = "c3")] Closed { #[store(child)] assignees: BTreeSet<String>, reason: String },
                #[store(code = "c1")] Draft,
                #[store(code = "c2")] Open {
                    #[store(child)] assignees: BTreeSet<String>,
                    #[store(intern = "wr")] owner: Option<String>,
                    #[store(flatten)] hold: Option<Hold>,
                    #[store(flatten)] signature: Signature,
                },
            }
        });
        for expected in [
            ":: flotilla_schema :: Labelled for TicketState",
            "type Label = TicketLabel",
            "Self :: Draft => & < TicketLabel > :: Draft",
            ":: flotilla_schema :: StateData for TicketState",
            "const CODES : & 'static [:: flotilla_schema :: Code] = & [const { :: flotilla_schema :: Code :: new (\"c1\") } , const { :: flotilla_schema :: Code :: new (\"c2\") } , const { :: flotilla_schema :: Code :: new (\"c3\") }]",
            "const DATA : & 'static [& 'static str] = & [\"assignees\" , \"reason\" , \"owner\" , \"hold\" , \"signature\"]",
            "layout . datum (\"assignees\" , label",
            "layout . tag_guard (label , const { :: flotilla_schema :: Code :: new (\"c3\") } , & datums , & [(0usize , true) , (1usize , true)])",
            "layout . tag_guard (label , const { :: flotilla_schema :: Code :: new (\"c1\") } , & datums , & [])",
            "layout . tag_guard (label , const { :: flotilla_schema :: Code :: new (\"c2\") } , & datums , & [(0usize , true) , (2usize , false) , (3usize , false) , (4usize , true)])",
            "< TicketLabel as :: flotilla_schema :: CodeEnum > :: code (& < TicketLabel > :: Open) , const { :: flotilla_schema :: Code :: new (\"c2\") }",
            "delta . table (\"assignees\"",
            "delta . column (\"owner\"",
            "adapt :: AsOptionalInterned as :: flotilla_schema :: adapt :: Adapter < Option < String > >",
            "Self :: Closed { assignees : __store_assignees , .. } => {",
            "Self :: Open { assignees : __store_assignees , .. } => {",
            "if code . as_str () == \"c3\"",
            "adapt :: AsOptionalFlat as :: flotilla_schema :: adapt :: Adapter < Option < Hold > >",
            "Hold : :: flotilla_schema :: Flat",
            "Signature : :: flotilla_schema :: Flat",
            "delta . column (\"signature\"",
        ] {
            assert!(state.contains(expected), "missing {expected}");
        }
        assert!(!state.contains("Columns for TicketState") && !state.contains("Element for TicketState"), "stored only as a State");
        // A code enum is its own label and a state carrying no data.
        let code = expanded(quote! { enum Phase { #[store(code = "c1")] A, #[store(code = "c2")] B } });
        assert!(code.contains("type Label = Self"), "{code}");
        assert!(code.contains("const DATA : & 'static [& 'static str] = & []"), "{code}");
    }

    // Behaviour (DESIGN indexes): `eq` and `order` name top-level single-column fields, `when`
    // allows only `<state field> in [..]` and `<optional field> is_some`, and an optional order
    // field must be required by `is_some`, and `order` refuses interned and reference fields
    // (local ids, ordered differently in every store) while `eq` accepts them. Everything else is
    // refused naming the list.
    #[test]
    fn index_refusals_name_the_closed_list() {
        let kind = |index: proc_macro2::TokenStream| {
            quote! {
                #[store(kind = "A", code = "k1", #index)]
                struct A {
                    a: Ref<B>,
                    b: Option<u64>,
                    #[store(intern = "tx")] c: String,
                    #[store(state)] s: State<Phase>,
                    d: Leaf,
                    #[store(child)] e: Vec<String>,
                    #[store(flatten)] f: Option<Leaf>,
                    g: Identity,
                    h: Option<::flotilla_schema::Ref<B>>,
                    #[store(intern = "tx")] i: Option<Name>,
                }
            }
        };
        let cases = [
            quote!(index(eq(a))),
            quote!(index(name = "x")),
            quote!(index(name = "Bad", eq(a))),
            quote!(index(name = "x", eq(nope))),
            quote!(index(name = "x", eq(e))),
            quote!(index(name = "x", eq(f))),
            quote!(index(name = "x", eq(s))),
            quote!(index(name = "x", eq(a), order(a))),
            quote!(index(name = "x", order(b))),
            quote!(index(name = "x", eq(a), when(a in [P]))),
            quote!(index(name = "x", eq(a), when(s in []))),
            quote!(index(name = "x", eq(a), when(s in [P, P]))),
            quote!(index(name = "x", eq(a), when(a is_some))),
            quote!(index(name = "x", eq(a), when(b is_none))),
            quote!(index(name = "x", eq(a), when(f is_some))),
            quote!(index(name = "x", eq(a), unique)),
            quote!(index(name = "x", eq(a), eq(c))),
            quote!(index(name = "x", order(a))),
            quote!(index(name = "x", order(c))),
            quote!(index(name = "x", order(g))),
            quote!(index(name = "x", order(h), when(h is_some))),
            quote!(index(name = "x", order(i), when(i is_some))),
            quote!(index(name = "x", eq(a)), index(name = "x", eq(c))),
        ];
        for case in cases {
            let message = error(kind(case.clone()));
            assert!(message.contains(SUPPORTED), "{case} was refused without naming the list: {message}");
        }
        assert!(expand(&syn::parse2(kind(quote!(index(name = "x", eq(a, c, g), order(b), when(b is_some))))).expect("parse")).is_ok());
        assert!(error(kind(quote!(index(name = "x", order(c))))).contains("cannot order an index"));
        assert!(error(quote! { #[store(index(name = "x", eq(a)))] struct A(String); }).contains("belongs on a kind or a part struct"));
        assert!(error(quote! { #[store(index(name = "x", eq(a)))] enum A { #[store(code = "c1")] B } })
            .contains("belongs on a kind or a part struct"));
    }

    // Glue: an index adds its descriptor entry to `table_set` with codes read through `CodeEnum`,
    // and a constructor whose arguments are the `eq` fields' types, inside `Option` when optional.
    #[test]
    fn indexes_expand_to_descriptors_and_constructors() {
        let expanded = expanded(quote! {
            #[store(kind = "Message", code = "k1",
                index(name = "open_by_inbox", eq(inbox), order(seq), when(state in [Pending, Awaiting])),
                index(name = "by_reply", eq(reply, sender), when(reply is_some)))]
            struct Message {
                inbox: Ref<Inbox>,
                seq: u64,
                reply: Option<Ref<Message>>,
                #[store(intern = "ag")] sender: Name,
                #[store(state)] state: State<MessageState>,
            }
        });
        for expected in [
            "< < MessageState as :: flotilla_schema :: Labelled > :: Label as :: flotilla_schema :: CodeEnum > :: code (& < < MessageState as :: flotilla_schema :: Labelled > :: Label > :: Pending)",
            "pub fn open_by_inbox (inbox : & Ref < Inbox >) -> :: flotilla_schema :: IndexQuery < Self >",
            "pub fn by_reply (reply : & Ref < Message > , sender : & Name)",
            ":: flotilla_schema :: interned_value :: < Name > (sender)",
            "Ref < Inbox > : :: flotilla_schema :: KeyColumn + :: flotilla_schema :: Element",
            ":: flotilla_schema :: Condition :: NotNull { column : :: std :: string :: String :: from (\"reply\") }",
        ] {
            assert!(expanded.contains(expected), "missing {expected}");
        }
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
        assert!(error(quote! { #[store(kind = "A")] struct A { a: String } }).contains("needs a stable `code = \"..\"`"));
        assert!(error(quote! { #[store(code = "c1")] struct A { a: String } }).contains("needs `kind"));
        assert!(error(quote! { #[store(kind = "snake_case", code = "k1")] struct A { a: String } }).contains("UpperCamelCase"));
        assert!(error(quote! { #[store(kind = "A", kind = "B", code = "k1")] struct A { a: String } }).contains("twice"));
    }

    // Glue: an accepted kind expands to the Element and Kind impls with per-field Column bounds.
    #[test]
    fn kinds_expand_with_per_field_bounds() {
        let input: DeriveInput = syn::parse2(quote! {
            #[store(kind = "Message", code = "k7")]
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
                #[store(intern = "tx")] a: String,
                #[store(intern = "tx")] b: Option<Name>,
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
            "Phase : :: flotilla_schema :: StateData",
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
            #[store(kind = "Message", code = "k1")]
            struct Message<T> { reply: Option<Ref<Message<T>>>, body: String, extra: T }
        });
        let kind = &expanded[expanded.find(":: flotilla_schema :: Kind for").expect("kind impl")..];
        let header = &kind[..kind.find('{').expect("body")];
        assert!(header.contains("T : :: flotilla_schema :: Column"));
        assert!(header.contains("Option < Ref < Message < T > > > : :: flotilla_schema :: Column"), "mentions T");
        assert!(!header.contains("String :"), "concrete fields are checked through Element only: {header}");
    }

    // Behaviour (DESIGN codes, #3392 G6): a code enum's CODES are ascending whatever the
    // declaration order, and a data enum's datums are named by the field alone below the prefix
    // (mapping B), with one tag guard per variant, unit variants included (they null every datum).
    #[test]
    fn enums_take_codes_and_shared_datums() {
        let code = expanded(quote! { enum Phase { #[store(code = "late")] Late, #[store(code = "early")] Early } });
        let early = "const { :: flotilla_schema :: Code :: new (\"early\") }";
        let late = "const { :: flotilla_schema :: Code :: new (\"late\") }";
        assert!(code.contains(&format!("const CODES : & 'static [:: flotilla_schema :: Code] = & [{early} , {late}]")), "{code}");
        let data = expanded(
            quote! { enum Edge { #[store(code = "dr")] DropIt { #[store(intern = "wr")] by: String }, #[store(code = "go")] Go } },
        );
        assert!(data.contains("\"by\"") && !data.contains("drop_it"), "{data}");
        assert!(
            data.contains(
                "layout . tag_guard (tag , const { :: flotilla_schema :: Code :: new (\"dr\") } , & datums , & [(0usize , true)])"
            ),
            "{data}"
        );
        assert!(
            data.contains("layout . tag_guard (tag , const { :: flotilla_schema :: Code :: new (\"go\") } , & datums , & [])"),
            "{data}"
        );
        assert!(data.contains("layout . expansion (tag , & datums)"), "{data}");
    }

    // Behaviour (#3320 ruling 1): every field of a struct gets an edge handle named by the field in
    // upper case, with the field's visibility: a `StateOf` for the state field, a `ColumnOf` for
    // another column-shaped field (its adapter encodes it; a `cbor` field's is `AsCbor`), a
    // `HistoryOf` or `RecordsOf` for a table of records, and none for a spec collection.
    // Column-shaped fields of a row are declared as its fields; a state field's `State` declares
    // its own label and data fields, and its handle knows whether the state carries data.
    #[test]
    fn fields_expand_to_edge_handles() {
        let kind = expanded(quote! {
            #[store(kind = "Message", code = "k1")]
            struct Message<T> {
                #[store(state)] state: State<Phase>,
                #[store(history)] pub edges: Records<Event<T>>,
                #[store(records)] notes: Records<Note>,
                #[store(child)] refs: Vec<Reference>,
                pub(crate) body: T,
                r#type: u8,
                #[store(cbor)] args: Args,
            }
        });
        for expected in [
            "const STATE : :: flotilla_schema :: StateOf < Self , State < Phase > > = :: flotilla_schema :: StateOf :: new (:: flotilla_schema :: ColumnOf :: new (\"state\"",
            "const ARGS : :: flotilla_schema :: ColumnOf < Self , Args >",
            "adapt :: AsCbor as :: flotilla_schema :: adapt :: Adapter < Args >",
            "Args : :: flotilla_schema :: Cbor",
            "pub const EDGES : :: flotilla_schema :: HistoryOf < Self , Event < T > > = :: flotilla_schema :: HistoryOf :: new (\"edges\")",
            "const NOTES : :: flotilla_schema :: RecordsOf < Self , Note >",
            "pub (crate) const BODY : :: flotilla_schema :: ColumnOf < Self , T >",
            "const TYPE : :: flotilla_schema :: ColumnOf < Self , u8 >",
            "< Phase as :: flotilla_schema :: StateData > :: DATA",
            "layout . field (\"body\" , false ,",
        ] {
            assert!(kind.contains(expected), "missing {expected}");
        }
        assert!(!kind.contains("layout . field (\"state\""), "a state's `State` records its label and data fields itself");
        assert!(!kind.contains("REFS"), "spec collections have no handle");
        assert!(!kind.contains("layout . field (\"edges\""), "table fields are not row fields");
    }

    // Behaviour: a handle never shadows a stored trait's associated item (an inherent const wins
    // over a trait's in `Message::KIND`): a kind's field named `kind` or `code`, or any struct's
    // field named `tables`, is refused naming the list; a part's field named `kind` is fine.
    #[test]
    fn handles_that_would_shadow_trait_items_are_refused() {
        for case in [
            quote! { #[store(kind = "A", code = "k1")] struct A { kind: String } },
            quote! { #[store(kind = "A", code = "k1")] struct A { code: u32 } },
            quote! { struct A { tables: u32 } },
            quote! { #[store(kind = "A", code = "k1")] struct A { r#tables: u32 } },
        ] {
            let message = error(case.clone());
            assert!(message.contains(SUPPORTED) && message.contains("edge handle"), "{case}: {message}");
        }
        assert!(expanded(quote! { struct Part { kind: Kind, code: u32 } }).contains("const KIND"));
    }

    // Behaviour (#3320 ruling of 2026-10-15): a part struct may declare indexes, laid out on the
    // table holding its rows (named when the layout reaches it), with no query constructor; a part
    // with indexes is never flattened, so it gets no `Flat`.
    #[test]
    fn part_indexes_go_on_the_table_holding_the_rows() {
        let part = expanded(quote! {
            #[store(index(name = "by_time", order(at)))]
            struct Event { at: Timestamp, edge: Edge }
        });
        assert!(part.contains("table : :: std :: string :: String :: from (layout . table_name ())"), "{part}");
        assert!(!part.contains("IndexQuery"), "no constructor");
        assert!(!part.contains("Flat for Event"));
        assert!(error(quote! { #[store(index(name = "x", order(nope)))] struct A { a: u8 } }).contains("not a field of this struct"));
    }

    // Behaviour: a newtype wrapping String is internable; other newtypes are not.
    #[test]
    fn newtypes_of_string_intern() {
        assert!(expanded(quote! { struct Name(String); }).contains(":: flotilla_schema :: Intern for Name"));
        assert!(!expanded(quote! { struct Count(u32); }).contains("Intern for"));
    }

    // Behaviour (#3392 ruling D5): a kind's spec (`state = S` beside `kind` and `code`) declares
    // the kind a generic holder stores: it is a row (`Element`) and a `KindSpec` naming its state,
    // with no `Kind` and no edge handles of its own; its indexes may name the holder's `state`, and
    // their queries carry the spec's kind code. A spec may be empty.
    #[test]
    fn specs_declare_a_holders_kind() {
        let spec = expanded(quote! {
            #[store(kind = "Message", code = "ms", state = MessageState,
                index(name = "open_by_inbox", eq(inbox), when(state in [Pending, Awaiting])))]
            struct MessageSpec {
                #[store(intern = "ad")] inbox: String,
                deadline: Option<Timestamp>,
            }
        });
        for expected in [
            ":: flotilla_schema :: Element for MessageSpec",
            ":: flotilla_schema :: KindSpec for MessageSpec",
            "type State = MessageState ;",
            "column : :: std :: string :: String :: from (\"state\")",
            "< < MessageState as :: flotilla_schema :: Labelled > :: Label > :: Pending",
            "IndexQuery :: new (< Self as :: flotilla_schema :: KindSpec > :: CODE",
        ] {
            assert!(spec.contains(expected), "missing {expected} in {spec}");
        }
        assert!(!spec.contains(":: flotilla_schema :: Kind for"), "the holder is the kind");
        assert!(!spec.contains("const INBOX"), "no edge handles on a spec");
        assert!(expanded(quote! { #[store(kind = "Session", code = "as", state = SessionState)] struct SessionSpec {} })
            .contains("KindSpec for SessionSpec"));
        for (case, problem) in [
            (quote! { #[store(state = S)] struct A { a: u8 } }, "needs `kind"),
            (quote! { #[store(kind = "A", code = "k1", state = S)] struct A { #[store(state)] s: State<S> } }, "no state field"),
            (quote! { #[store(kind = "A", code = "k1", state = S)] struct A { state: u8 } }, "rename this field"),
            (quote! { #[store(state = S)] enum A { #[store(code = "c1")] B } }, "spec struct"),
            (quote! { #[store(kind = "A", code = "k1", index(name = "x", eq(a), when(state in [B])))] struct A { a: u8 } }, "not a field"),
        ] {
            let message = error(case.clone());
            assert!(message.contains(SUPPORTED) && message.contains(problem), "{case}: {message}");
        }
    }
    // Behaviour (#3422): explicit short domain codes are required, and a shared datum must
    // agree on its domain in ordinary enums and label states, regardless of Rust type.
    #[test]
    fn intern_domains_are_required_validated_and_agreed() {
        for domain in ["", " ", "UP", "1a", "é", "a-b", "abcdefghi"] {
            let case = quote! { struct A { #[store(intern = #domain)] a: String } };
            assert!(error(case).contains("a code is short text"));
        }
        assert!(error(quote! { struct A { #[store(intern)] a: String } }).contains("intern"));
        for labels in [quote! {}, quote! { #[store(labels = L)] }] {
            let mismatch = quote! {
                #labels enum A {
                    #[store(code = "b")] B { #[store(intern = "ad")] a: String },
                    #[store(code = "c")] C { #[store(intern = "wr")] a: Name },
                }
            };
            assert!(error(mismatch).contains("same intern domain"));
            // Both boundaries of the existing Code grammar are valid; domains are not a closed list.
            for domain in ["a", "a_123456"] {
                let _ = expanded(quote! {
                    #labels enum A {
                        #[store(code = "b")] B { #[store(intern = #domain)] a: String },
                        #[store(code = "c")] C { #[store(intern = #domain)] a: Option<Name> },
                    }
                });
            }
        }
    }
}
