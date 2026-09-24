//
// This source file is part of the Pylon open source project.
//
// Copyright (c) 2026 Jaldis B.V.
//
// Licensed under the MIT OR Apache-2.0 license (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://opensource.org/licenses/MIT
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

//! `#[derive(Queryable)]` — populates a struct or enum from a query result,
//! so a caller writes `client.query::<Row, _>(pyql, &args)` instead of
//! walking a generic [`Value`](pylon_client::Value) by hand.
//!
//! Deliberately mirrors `the upstream derive crate`'s surface (same derive name, same
//! `rename`/`json`/`crate_path` attributes under a `pylon` namespace), so a
//! project moving off the upstream engine's Rust client keeps its row structs as-is. The one
//! semantic difference is spelled out on
//! [`Queryable`](pylon_client::Queryable): fields are matched **by name**
//! rather than by shape position, because Pylon's decoded objects are
//! name-keyed.

use proc_macro::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, Ident, LitStr, Path, parse_macro_input};

/// Attributes accepted on the container, a field or an enum variant.
#[derive(Default)]
struct Attrs {
    rename: Option<LitStr>,
    json: bool,
    crate_path: Option<Path>,
}

fn parse_attrs(attrs: &[syn::Attribute]) -> syn::Result<Attrs> {
    let mut parsed = Attrs::default();
    for attr in attrs.iter().filter(|a| a.path().is_ident("pylon")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("json") {
                parsed.json = true;
                Ok(())
            } else if meta.path.is_ident("rename") {
                parsed.rename = Some(meta.value()?.parse()?);
                Ok(())
            } else if meta.path.is_ident("crate_path") {
                parsed.crate_path = Some(meta.value()?.parse()?);
                Ok(())
            } else {
                Err(meta.error("unknown `pylon` attribute; expected `json`, `rename` or `crate_path`"))
            }
        })?;
    }
    Ok(parsed)
}

#[proc_macro_derive(Queryable, attributes(pylon))]
pub fn queryable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input).unwrap_or_else(|error| error.to_compile_error()).into()
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let container = parse_attrs(&input.attrs)?;
    let crate_path = container
        .crate_path
        .clone()
        .unwrap_or_else(|| syn::parse_quote!(::pylon_client));
    let name = &input.ident;
    let name_literal = name.to_string();
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    // A container-level `json` deserializes the whole value with serde and
    // never looks at the Rust shape at all, so it applies to structs and
    // enums alike.
    let body = if container.json {
        quote! { #crate_path::queryable::derive::from_json(value, #name_literal) }
    } else {
        match &input.data {
            Data::Struct(data) => struct_body(&crate_path, &name_literal, &data.fields)?,
            Data::Enum(data) => enum_body(&crate_path, name, &name_literal, data)?,
            Data::Union(_) => {
                return Err(syn::Error::new(
                    input.span(),
                    "Queryable cannot be derived for a union; use a struct or an enum",
                ));
            }
        }
    };

    Ok(quote! {
        impl #impl_generics #crate_path::Queryable for #name #type_generics #where_clause {
            fn decode(
                value: &#crate_path::Value,
            ) -> ::core::result::Result<Self, #crate_path::DecodeError> {
                #body
            }
        }
    })
}

fn struct_body(crate_path: &Path, name_literal: &str, fields: &Fields) -> syn::Result<proc_macro2::TokenStream> {
    let Fields::Named(named) = fields else {
        return Err(syn::Error::new(
            fields.span(),
            "Queryable needs named fields — a query shape is a set of named pointers, \
             so a tuple or unit struct has nothing to match them against",
        ));
    };

    let mut initializers = Vec::new();
    for field in &named.named {
        let attrs = parse_attrs(&field.attrs)?;
        // `ident` is always `Some` inside `Fields::Named`.
        let ident = field.ident.as_ref().expect("named field has an identifier");
        let shape_name = attrs
            .rename
            .as_ref()
            .map(LitStr::value)
            .unwrap_or_else(|| ident.to_string());
        let helper = if attrs.json { quote!(json_field) } else { quote!(field) };
        initializers.push(quote! {
            #ident: #crate_path::queryable::derive::#helper(object, #name_literal, #shape_name)?
        });
    }

    Ok(quote! {
        let object = #crate_path::queryable::derive::object(value, #name_literal)?;
        ::core::result::Result::Ok(Self { #(#initializers),* })
    })
}

fn enum_body(
    crate_path: &Path,
    name: &Ident,
    name_literal: &str,
    data: &syn::DataEnum,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut arms = Vec::new();
    for variant in &data.variants {
        if !matches!(variant.fields, Fields::Unit) {
            return Err(syn::Error::new(
                variant.span(),
                "Queryable only supports unit variants — a schema enum's members carry no payload",
            ));
        }
        let attrs = parse_attrs(&variant.attrs)?;
        if attrs.json {
            return Err(syn::Error::new(
                variant.span(),
                "`json` is not valid on a variant; put it on the container or on a struct field",
            ));
        }
        let ident = &variant.ident;
        let label = attrs
            .rename
            .as_ref()
            .map(LitStr::value)
            .unwrap_or_else(|| ident.to_string());
        arms.push(quote! { #label => ::core::result::Result::Ok(#name::#ident) });
    }

    Ok(quote! {
        match #crate_path::queryable::derive::enum_label(value, #name_literal)? {
            #(#arms,)*
            other => ::core::result::Result::Err(
                #crate_path::queryable::derive::unknown_variant(#name_literal, other),
            ),
        }
    })
}
