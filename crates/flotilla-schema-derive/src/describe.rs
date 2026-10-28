//! Describe-only expansion: shape attributes and Layout, with no stored trait implementations.
use crate::serde_names::{serialization_name, snake_case, RenameRule};
use proc_macro2::TokenStream;
use quote::quote;
use syn::{ext::IdentExt, Data, DeriveInput, Fields, LitStr};

fn shape(field: &syn::Field) -> syn::Result<crate::attrs::FieldAttrs> {
    if let Some(attr) = field.attrs.iter().find(|attr| attr.path().is_ident("store")) {
        return Err(syn::Error::new_spanned(attr, "describe-only fields use #[shape(...)], not storage attributes"));
    }
    crate::attrs::described_field_attrs(&field.attrs)
}

fn field(field: &syn::Field, prefix: TokenStream, fallback: &str) -> syn::Result<TokenStream> {
    let name = field.ident.as_ref().map(|ident| ident.unraw().to_string()).unwrap_or_else(|| fallback.into());
    let ty = &field.ty;
    let name = quote!(&::flotilla_schema::Layout::join(#prefix, #name));
    let attrs = shape(field)?;
    if attrs.key.is_some() {
        if crate::shape::option_inner(ty).is_some() || attrs.shape == Some(crate::attrs::FieldAttr::Child) {
            return Err(syn::Error::new_spanned(field, "a row key is a required column-shaped field, not an option or a child table"));
        }
        return Ok(quote!(layout.key(|layout| <#ty as ::flotilla_schema::Describe>::describe(layout, #name, true));));
    }
    Ok(match attrs.shape {
        Some(crate::attrs::FieldAttr::Child) => quote!(<#ty as ::flotilla_schema::DescribeChild>::describe_child(layout, #name);),
        Some(crate::attrs::FieldAttr::Intern) => {
            let domain = attrs.domain.expect("intern has a domain");
            let inner = crate::shape::option_inner(ty).unwrap_or(ty);
            let optional = crate::shape::option_inner(ty).is_some();
            quote!({
                fn assert_intern<T: ::flotilla_schema::Intern>() {}
                assert_intern::<#inner>();
                layout.column(#name, ::flotilla_schema::ColumnType::Interned(const { ::flotilla_schema::Code::new(#domain) }), required && !#optional);
            })
        }
        Some(crate::attrs::FieldAttr::Flatten) | None => quote!(<#ty as ::flotilla_schema::Describe>::describe(layout, #name, required);),
        _ => return Err(syn::Error::new_spanned(field, "Describe accepts shape attributes only")),
    })
}

pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    if let Some(attr) = input.attrs.iter().find(|attr| attr.path().is_ident("store")) {
        return Err(syn::Error::new_spanned(attr, "Describe accepts shape attributes only"));
    }
    let body = match &input.data {
        Data::Struct(data) => {
            if matches!(&data.fields, Fields::Unnamed(fields) if fields.unnamed.len() != 1) {
                return Err(syn::Error::new_spanned(&data.fields, "use named fields or a single-field newtype"));
            }
            let fields = data.fields.iter().map(|f| field(f, quote!(name), "")).collect::<syn::Result<Vec<_>>>()?;
            quote!(#(#fields)*)
        }
        Data::Enum(data) => {
            if data.variants.is_empty() {
                return Err(syn::Error::new_spanned(input, "an enum needs at least one variant"));
            }
            let rename_all = RenameRule::parse(serialization_name(&input.attrs, "rename_all")?)?;
            let mut categories = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            let mut datums: Vec<(String, syn::Field, String)> = Vec::new();
            let mut carried = Vec::new();
            for variant in &data.variants {
                let label = variant.ident.unraw().to_string();
                let value =
                    serialization_name(&variant.attrs, "rename")?.map(|name| name.value()).unwrap_or_else(|| rename_all.apply(&label));
                for attr in &variant.attrs {
                    if attr.path().is_ident("store") {
                        return Err(syn::Error::new_spanned(attr, "Describe accepts shape attributes only"));
                    }
                    if attr.path().is_ident("shape") {
                        for shape in crate::attrs::validate_shape_attr(attr)? {
                            let crate::attrs::ShapeAttr::Code(code) = shape else {
                                return Err(syn::Error::new_spanned(attr, "expected code"));
                            };
                            if code.value() != value {
                                return Err(syn::Error::new_spanned(
                                    code,
                                    format!("a described variant's code is the value Serde writes, `{value}`; rename it with #[serde(rename = ...)]"),
                                ));
                            }
                        }
                    }
                }
                if !seen.insert(value.clone()) {
                    return Err(syn::Error::new_spanned(variant, "enum values must be distinct"));
                }
                if matches!(&variant.fields, Fields::Unnamed(fields) if fields.unnamed.len() != 1) {
                    return Err(syn::Error::new_spanned(variant, "use named fields or a single-field variant"));
                }
                categories.push(quote!((#value.into(), #label.into())));
                let mut variant_datums = Vec::new();
                for datum in &variant.fields {
                    let fallback = snake_case(&label);
                    let key = datum.ident.as_ref().map(|ident| ident.unraw().to_string()).unwrap_or(fallback);
                    let inner = crate::shape::option_inner(&datum.ty).unwrap_or(&datum.ty);
                    let attrs = shape(datum)?;
                    if attrs.key.is_some() {
                        return Err(syn::Error::new_spanned(datum, "a row key belongs on a struct field"));
                    }
                    let domain = attrs.domain.as_ref().map(LitStr::value);
                    let signature = format!("{} {:?} {:?}", quote!(#inner), attrs.shape, domain);
                    let optional = crate::shape::option_inner(&datum.ty).is_some();
                    let index = if let Some(index) = datums.iter().position(|(name, _, _)| name == &key) {
                        if datums[index].2 != signature {
                            return Err(syn::Error::new_spanned(datum, "a shared datum must have one type and shape across variants"));
                        }
                        if optional {
                            datums[index].1 = datum.clone();
                        }
                        index
                    } else {
                        datums.push((key, datum.clone(), signature));
                        datums.len() - 1
                    };
                    variant_datums.push((index, !optional));
                }
                carried.push((value, variant_datums));
            }
            let has_data = !datums.is_empty();
            let layouts = datums
                .iter()
                .map(|(key, datum, _)| {
                    let layout = field(datum, quote!(name), key)?;
                    let mut required_field = datum.clone();
                    let inner = crate::shape::option_inner(&datum.ty);
                    let required = if let Some(inner) = inner {
                        required_field.ty = inner.clone();
                        let required = field(&required_field, quote!(name), key)?;
                        quote! {
                            let required = ::flotilla_schema::Layout::required_of(|layout| { #required });
                            ::flotilla_schema::DatumColumns { required, ..datum }
                        }
                    } else {
                        quote!(datum)
                    };
                    Ok(quote!({ let datum = layout.scope(|layout| { #layout }); #required }))
                })
                .collect::<syn::Result<Vec<_>>>()?;
            let guards = carried.iter().map(|(value, carried)| {
                let indexes = carried.iter().map(|(index, required)| quote!((#index, #required)));
                quote!(layout.tag_guard(&tag, #value, &datums, &[#(#indexes),*]);)
            });
            quote! {
                let tag = if #has_data && name.is_empty() { "tag" } else { name };
                layout.categories(tag, vec![#(#categories),*], required);
                let datums = vec![#(#layouts),*];
                #(#guards)*
                if !datums.is_empty() { layout.expansion(tag, &datums); }
            }
        }
        Data::Union(data) => return Err(syn::Error::new_spanned(data.union_token, "unions have no table description")),
    };
    let ident = &input.ident;
    let mut generics = input.generics.clone();
    let parameters: Vec<_> = generics.type_params().map(|parameter| parameter.ident.clone()).collect();
    for parameter in parameters {
        generics.make_where_clause().predicates.push(syn::parse_quote!(#parameter: ::flotilla_schema::Describe));
    }
    let (imp, ty, clause) = generics.split_for_impl();
    Ok(quote! {
        #[automatically_derived]
        impl #imp ::flotilla_schema::Describe for #ident #ty #clause {
            fn describe(layout: &mut ::flotilla_schema::Layout, name: &str, required: bool) {
                #body
            }
            fn table_set() -> ::flotilla_schema::TableSet {
                let mut layout = ::flotilla_schema::Layout::description(stringify!(#ident));
                <Self as ::flotilla_schema::Describe>::describe(&mut layout, "", true);
                layout.finish()
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::expand;
    use quote::quote;

    // ADR 0067: describe-only expansion emits the descriptor trait, never a codec or handles.
    // Refusals enumerate every storage-only field attribute and both misplaced namespaces.
    #[test]
    fn describe_emits_only_description_and_refuses_storage_attributes() {
        let input = syn::parse2(quote!(
            struct Row {
                value: String,
            }
        ))
        .unwrap();
        let tokens = expand(&input).unwrap().to_string();
        assert!(tokens.contains("Describe for Row"));
        for forbidden in ["Encoder", "Decoder", "Element", "Kind", "ColumnOf"] {
            assert!(!tokens.contains(forbidden));
        }
        for storage in ["records", "history", "frozen", "cbor", "state"] {
            let attr = syn::Ident::new(storage, proc_macro2::Span::call_site());
            let input = syn::parse2(quote!(
                struct Row {
                    #[shape(#attr)]
                    value: String,
                }
            ))
            .unwrap();
            assert!(expand(&input).is_err(), "{storage}");
        }
        for input in [
            quote!(
                #[store(kind = "Row", code = "rw")]
                struct Row {
                    value: String,
                }
            ),
            quote!(
                struct Row {
                    #[store(child)]
                    value: Vec<String>,
                }
            ),
        ] {
            assert!(expand(&syn::parse2(input).unwrap()).is_err());
        }
    }

    #[test]
    fn a_described_variant_code_is_its_serialized_name() {
        let accepted = quote!(
            #[serde(rename_all = "snake_case")]
            enum Phase {
                #[shape(code = "in_flight")]
                InFlight,
                #[serde(rename = "z")]
                #[shape(code = "z")]
                Done,
            }
        );
        assert!(expand(&syn::parse2(accepted).unwrap()).is_ok());
        let refused = quote!(
            enum Phase {
                #[shape(code = "a")]
                Active,
            }
        );
        let error = expand(&syn::parse2(refused).unwrap()).unwrap_err().to_string();
        assert!(error.contains("`Active`"), "{error}");
    }

    #[test]
    fn a_row_key_is_a_required_struct_field_of_a_described_row() {
        let accepted = quote!(
            struct Row {
                #[shape(key)]
                id: String,
            }
        );
        assert!(expand(&syn::parse2(accepted.clone()).unwrap()).is_ok());
        let error = crate::expand(&syn::parse2(accepted).unwrap()).unwrap_err().to_string();
        assert!(error.contains("`key` declares a described row's key"), "{error}");
        for refused in [
            quote!(
                struct Row {
                    #[shape(key)]
                    id: Option<String>,
                }
            ),
            quote!(
                struct Row {
                    #[shape(key, child)]
                    ids: Vec<String>,
                }
            ),
            quote!(
                enum Row {
                    One {
                        #[shape(key)]
                        id: String,
                    },
                }
            ),
        ] {
            assert!(expand(&syn::parse2(refused).unwrap()).is_err());
        }
    }
}
