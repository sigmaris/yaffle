extern crate proc_macro;
use proc_macro2::{Ident, Span, TokenStream};
use quote::{quote, quote_spanned};
use syn::{
    parse_macro_input, spanned::Spanned, Data, DataStruct, DeriveInput, Fields, FieldsNamed, Lit,
    Meta, MetaNameValue, NestedMeta,
};

fn gelf_accessor(ident: &Ident, field: &str) -> TokenStream {
    if field == "version" || field == "host" || field == "short_message" {
        let field_ident = Ident::new(field, Span::call_site());
        quote! { ::std::option::Option::Some(::serde_json::Value::String(#ident.#field_ident.to_owned())) }
    } else {
        quote! { #ident.other.get(#field) }
    }
}

fn constructor_expr(
    dest_field: &Ident,
    gelf_ident: &Ident,
    gelf_fields: &Vec<FieldValueConversion>,
) -> TokenStream {
    let mut expr = None;
    for gelf_field in gelf_fields {
        let accessor = gelf_field.conversion_expr(&gelf_ident);
        expr = expr
            .map(|existing| quote! { #existing.or(#accessor) })
            .or(Some(accessor));
    }
    let final_expr = expr.unwrap_or(quote! { None });
    quote! { #dest_field : #final_expr }
}

#[derive(Debug)]
enum FieldValueConversion {
    None(String),
    FloatSecToUsec(String),
    HexToUint(String),
    SyslogTimestamp(String),
}

impl FieldValueConversion {
    fn from_attribute(fieldname: String, conversion: &str) -> Option<Self> {
        match conversion {
            "none" => Some(Self::None(fieldname)),
            "float_sec_to_usec" => Some(Self::FloatSecToUsec(fieldname)),
            "hex_to_uint" => Some(Self::HexToUint(fieldname)),
            "syslog_timestamp" => Some(Self::SyslogTimestamp(fieldname)),
            _ => None,
        }
    }

    fn conversion_expr(&self, gelf_ident: &Ident) -> TokenStream {
        match self {
            Self::None(field) => {
                let accessor = gelf_accessor(gelf_ident, field);
                quote! { #accessor.map(|val| ::serde_json::value::from_value(val.clone())).transpose()? }
            }
            Self::FloatSecToUsec(field) => {
                let accessor = gelf_accessor(gelf_ident, field);
                quote! { #accessor.map(|val| Self::convert_float_sec_to_usec(val)).transpose()? }
            }
            Self::HexToUint(field) => {
                let accessor = gelf_accessor(gelf_ident, field);
                quote! { #accessor.map(|val| Self::convert_hex_to_uint(val)).transpose()? }
            }
            Self::SyslogTimestamp(field) => {
                let accessor = gelf_accessor(gelf_ident, field);
                quote! { #accessor.map(|val| Self::convert_syslog_timestamp(val)).transpose()? }
            }
        }
    }
}

#[proc_macro_derive(YaffleSchema, attributes(from_gelf, gelf_convert))]
pub fn derive_yaffle_schema(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let input_span = input.span();
    let name = input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let mut fields: Vec<(Ident, Vec<FieldValueConversion>)> = vec![];
    match input.data {
        Data::Struct(DataStruct {
            fields: Fields::Named(FieldsNamed { named, .. }),
            ..
        }) => {
            for field in named {
                let mut from_gelf: Vec<FieldValueConversion> =
                    Vec::with_capacity(field.attrs.len());
                for attr in field.attrs {
                    match attr.parse_meta() {
                        Ok(Meta::List(list)) if list.path.is_ident("from_gelf") => {
                            for gelf_field in list.nested.iter().filter_map(|inner| match inner {
                                NestedMeta::Meta(Meta::Path(inner_path)) => {
                                    // TODO: check error coming from from_attribute
                                    inner_path
                                        .get_ident()
                                        .map(|i| {
                                            FieldValueConversion::from_attribute(
                                                i.to_string(),
                                                "none",
                                            )
                                        })
                                        .flatten()
                                }
                                NestedMeta::Meta(Meta::NameValue(MetaNameValue {
                                    path,
                                    lit: Lit::Str(conv),
                                    ..
                                })) => path
                                    .get_ident()
                                    .map(|i| {
                                        FieldValueConversion::from_attribute(
                                            i.to_string(),
                                            &conv.value(),
                                        )
                                    })
                                    .flatten(),
                                NestedMeta::Lit(Lit::Str(lit_str)) => {
                                    FieldValueConversion::from_attribute(lit_str.value(), "none")
                                }
                                _ => {
                                    None /* TODO: should really error */
                                }
                            }) {
                                from_gelf.push(gelf_field);
                            }
                        }
                        _ => {
                            return quote_spanned!{input_span=> compile_error!("This macro must be used on a struct with named fields")}.into();
                        }
                    }
                }
                fields.push((field.ident.unwrap(), from_gelf));
            }
        }
        _ => {}
    }

    let construct_exprs: Vec<TokenStream> = fields
        .iter()
        .map(|(field, from_gelfs)| {
            constructor_expr(field, &Ident::new("msg", Span::call_site()), from_gelfs)
        })
        .collect();

    let expanded = quote! {
        impl #impl_generics crate::YaffleSchema for #name #ty_generics #where_clause {
            fn from_gelf(msg: &crate::GELFMessage) -> ::std::result::Result<#name, ::std::boxed::Box<dyn ::std::error::Error>> {
                Ok(#name {
                    #( #construct_exprs ),*
                })
            }
        }
    };

    // Hand the output tokens back to the compiler
    expanded.into()
}
