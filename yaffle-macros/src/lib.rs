extern crate proc_macro;
use std::collections::HashMap;

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

fn syslog_accessor(ident: &Ident, field: &str) -> TokenStream {
    let field_ident = Ident::new(field, Span::call_site());
    if field == "priority"
        || field == "facility"
        || field == "source_timestamp"
        || field == "message"
        || field == "full_message"
    {
        quote! { ::std::option::Option::Some(#ident.#field_ident.clone().into()) }
    } else {
        quote! { #ident.#field_ident.clone().into() }
    }
}

fn constructor_expr(
    dest_field: &Ident,
    source_ident: &Ident,
    source_fields: &[FieldValueConversion],
    syslog: bool,
) -> TokenStream {
    let mut expr = None;
    for source_field in source_fields {
        let accessor = source_field.conversion_expr(source_ident, syslog);
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
    DateTimeToUsec(String),
}

impl FieldValueConversion {
    fn from_attribute(fieldname: String, conversion: &str) -> Option<Self> {
        match conversion {
            "none" => Some(Self::None(fieldname)),
            "float_sec_to_usec" => Some(Self::FloatSecToUsec(fieldname)),
            "hex_to_uint" => Some(Self::HexToUint(fieldname)),
            "syslog_timestamp" => Some(Self::SyslogTimestamp(fieldname)),
            "datetime_to_usec" => Some(Self::DateTimeToUsec(fieldname)),
            _ => None,
        }
    }

    fn conversion_expr(&self, source_ident: &Ident, syslog: bool) -> TokenStream {
        match self {
            Self::None(field) => {
                if syslog {
                    syslog_accessor(source_ident, field)
                } else {
                    let accessor = gelf_accessor(source_ident, field);
                    quote! { #accessor.map(|val| {
                        let prim_result = ::serde_json::value::from_value(val.clone());
                        if prim_result.is_err() {
                            if let ::serde_json::value::Value::String(ref some_str) = val {
                                return some_str.parse().or(prim_result)
                            }
                        }
                        prim_result
                    }).transpose()? }
                }
            }
            Self::FloatSecToUsec(field) => {
                let accessor = if syslog {
                    let acc1 = syslog_accessor(source_ident, field);
                    quote! { #acc1.map(::serde_json::to_value) }
                } else {
                    gelf_accessor(source_ident, field)
                };
                quote! { #accessor.map(|val| Self::convert_float_sec_to_usec(val)).transpose()? }
            }
            Self::HexToUint(field) => {
                let accessor = if syslog {
                    let acc1 = syslog_accessor(source_ident, field);
                    quote! { #acc1.map(::serde_json::to_value) }
                } else {
                    gelf_accessor(source_ident, field)
                };
                quote! { #accessor.map(|val| Self::convert_hex_to_uint(val)).transpose()? }
            }
            Self::SyslogTimestamp(field) => {
                let accessor = if syslog {
                    let acc1 = syslog_accessor(source_ident, field);
                    quote! { #acc1.map(::serde_json::to_value) }
                } else {
                    gelf_accessor(source_ident, field)
                };
                quote! { #accessor.map(|val| Self::convert_syslog_timestamp(val)).transpose()? }
            }
            Self::DateTimeToUsec(field) => {
                let accessor = if syslog {
                    syslog_accessor(source_ident, field)
                } else {
                    gelf_accessor(source_ident, field)
                };
                quote! { #accessor.map(|val| Self::convert_datetime_to_usec(val)) }
            }
        }
    }
}

#[derive(Debug)]
enum TantivyType {
    String,
    Text,
    U64,
    I64,
    Timestamp,
    FastTimestamp,
    JsonObject,
}

impl TantivyType {
    fn from_attribute(attr: &str) -> Option<Self> {
        match attr {
            "string" => Some(Self::String),
            "text" => Some(Self::Text),
            "u64" => Some(Self::U64),
            "i64" => Some(Self::I64),
            "timestamp" => Some(Self::Timestamp),
            "fast_timestamp" => Some(Self::FastTimestamp),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum FormatOption {
    SyslogPriority,
    Hex,
}

impl FormatOption {
    fn from_attribute(attr: &str) -> Option<Self> {
        match attr {
            "syslog_priority" => Some(Self::SyslogPriority),
            "hex" => Some(Self::Hex),
            _ => None,
        }
    }
}

fn process_inner_attr(inner: &NestedMeta) -> Option<FieldValueConversion> {
    match inner {
        NestedMeta::Meta(Meta::Path(inner_path)) => {
            // TODO: check error coming from from_attribute
            inner_path
                .get_ident()
                .and_then(|i| FieldValueConversion::from_attribute(i.to_string(), "none"))
        }
        NestedMeta::Meta(Meta::NameValue(MetaNameValue {
            path,
            lit: Lit::Str(conv),
            ..
        })) => path
            .get_ident()
            .and_then(|i| FieldValueConversion::from_attribute(i.to_string(), &conv.value())),
        NestedMeta::Lit(Lit::Str(lit_str)) => {
            FieldValueConversion::from_attribute(lit_str.value(), "none")
        }
        _ => {
            None /* TODO: should really error */
        }
    }
}

#[proc_macro_derive(YaffleSchema, attributes(from_gelf, from_syslog, storage_type, format))]
pub fn derive_yaffle_schema(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let input_span = input.span();
    let name = input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let mut from_gelf_fields: Vec<(Ident, Vec<FieldValueConversion>)> = vec![];
    let mut from_syslog_fields: Vec<(Ident, Vec<FieldValueConversion>)> = vec![];
    let mut tantivy_fields: Vec<(String, TantivyType)> = vec![];
    let mut format_options: HashMap<String, FormatOption> = HashMap::new();
    match input.data {
        Data::Struct(DataStruct {
            fields: Fields::Named(FieldsNamed { named, .. }),
            ..
        }) => {
            for field in named {
                let mut from_gelf: Vec<FieldValueConversion> = Vec::new();
                let mut from_syslog: Vec<FieldValueConversion> = Vec::new();
                for attr in field.attrs {
                    match attr.parse_meta() {
                        Ok(Meta::List(list)) if list.path.is_ident("from_gelf") => {
                            for gelf_field in list.nested.iter().filter_map(process_inner_attr) {
                                from_gelf.push(gelf_field);
                            }
                        }
                        Ok(Meta::List(list)) if list.path.is_ident("from_syslog") => {
                            for syslog_field in list.nested.iter().filter_map(process_inner_attr) {
                                from_syslog.push(syslog_field);
                            }
                        }
                        Ok(Meta::NameValue(MetaNameValue {
                            path,
                            lit: Lit::Str(typename),
                            ..
                        })) if path.is_ident("storage_type") => tantivy_fields.push((
                            field.ident.as_ref().unwrap().to_string(),
                            // TODO detect if it's an Option
                            TantivyType::from_attribute(&typename.value()).unwrap(),
                        )),
                        Ok(Meta::NameValue(MetaNameValue {
                            path,
                            lit: Lit::Str(fmt),
                            ..
                        })) if path.is_ident("format") => {
                            format_options.insert(
                                field.ident.as_ref().unwrap().to_string(),
                                FormatOption::from_attribute(&fmt.value()).unwrap(),
                            );
                        }
                        _ => {}
                    }
                }
                from_gelf_fields.push((field.ident.clone().unwrap(), from_gelf));
                from_syslog_fields.push((field.ident.unwrap(), from_syslog));
            }
        }
        _ => {
            return quote_spanned!{input_span=> compile_error!("This macro must be used on a struct with named fields")}.into();
        }
    }

    let gelf_construct_exprs: Vec<TokenStream> = from_gelf_fields
        .iter()
        .map(|(field, from_gelfs)| {
            constructor_expr(
                field,
                &Ident::new("gelf_msg", Span::call_site()),
                from_gelfs,
                false,
            )
        })
        .collect();
    let syslog_construct_exprs: Vec<TokenStream> = from_syslog_fields
        .iter()
        .map(|(field, from_syslogs)| {
            constructor_expr(
                field,
                &Ident::new("syslog_msg", Span::call_site()),
                from_syslogs,
                true,
            )
        })
        .collect();

    let schema_builder_stmts: Vec<TokenStream> = tantivy_fields.iter().map(|(field, tantivy_type)| match *tantivy_type {
        TantivyType::String => quote!{ schema_builder.add_text_field(#field, ::tantivy::schema::STORED | ::tantivy::schema::STRING) },
        TantivyType::Text => quote!{ schema_builder.add_text_field(#field, ::tantivy::schema::STORED | ::tantivy::schema::TEXT) },
        TantivyType::U64 => quote!{ schema_builder.add_u64_field(#field, ::tantivy::schema::STORED | ::tantivy::schema::INDEXED) },
        TantivyType::I64 => quote!{ schema_builder.add_i64_field(#field, ::tantivy::schema::STORED | ::tantivy::schema::INDEXED) },
        TantivyType::Timestamp => quote!{ schema_builder.add_date_field(#field, ::tantivy::schema::STORED | ::tantivy::schema::INDEXED) },
        TantivyType::FastTimestamp => quote!{ schema_builder.add_date_field(#field, ::tantivy::schema::STORED | ::tantivy::schema::INDEXED | ::tantivy::schema::FAST) },
        TantivyType::JsonObject => quote!{ schema_builder.add_json_field(#field, ::tantivy::schema::STORED | ::tantivy::schema::INDEXED) },
    }).collect();

    let quickwit_document_exprs: Vec<TokenStream> = tantivy_fields
        .iter()
        .map(|(field, tantivy_type)| match *tantivy_type {
            TantivyType::String => {
                quote! { crate::quickwit::FieldMapping {
                    name: #field.to_string(),
                    type_: "text".to_string(),
                    tokenizer: Some(crate::quickwit::Tokenizer::Raw),
                    ..Default::default()
                } }
            }
            TantivyType::Text => quote! { crate::quickwit::FieldMapping {
                name: #field.to_string(),
                type_: "text".to_string(),
                record: Some(crate::quickwit::RecordOption::Position),
                ..Default::default()
            } },
            TantivyType::U64 => quote! { crate::quickwit::FieldMapping {
                name: #field.to_string(),
                type_: "u64".to_string(),
                ..Default::default()
            } },
            TantivyType::I64 => quote! { crate::quickwit::FieldMapping {
                name: #field.to_string(),
                type_: "i64".to_string(),
                ..Default::default()
            } },
            TantivyType::Timestamp => {
                quote! { crate::quickwit::FieldMapping {
                    name: #field.to_string(),
                    type_: "datetime".to_string(),
                    input_formats: vec!["unix_timestamp".to_string()],
                    fast: false,
                    precision: Some(crate::quickwit::DatePrecision::Microseconds),
                    ..Default::default()
                } }
            }
            TantivyType::FastTimestamp => {
                quote! { crate::quickwit::FieldMapping {
                    name: #field.to_string(),
                    type_: "datetime".to_string(),
                    input_formats: vec!["unix_timestamp".to_string()],
                    fast: true,
                    precision: Some(crate::quickwit::DatePrecision::Microseconds),
                    ..Default::default()
                } }
            }
            TantivyType::JsonObject => {
                quote! { crate::quickwit::FieldMapping {
                    name: #field.to_string(),
                    type_: "json",
                    ..Default::default()
                } }
            }
        })
        .collect();

    // Statements to convert document to a map of fieldname->str for display
    let to_display_stmts: Vec<TokenStream> = tantivy_fields.iter().map(|(field, tantivy_type)| {
        let accessor = match *tantivy_type {
            TantivyType::String | TantivyType::Text => quote!{ value.into() },
            TantivyType::U64 | TantivyType::I64 => match format_options.get(field) {
                Some(FormatOption::SyslogPriority) => quote!{
                    format!("{} ({})", value, match value {
                        0 => "Emergency",
                        1 => "Alert",
                        2 => "Critical",
                        3 => "Error",
                        4 => "Warning",
                        5 => "Notice",
                        6 => "Informational",
                        7 => "Debug",
                        _ => "Unknown",
                    }).into()
                },
                Some(FormatOption::Hex) => quote!{ format!("0x{:x}", value).into() },
                None => quote!{ value.to_string().into() },
            },
            TantivyType::Timestamp | TantivyType::FastTimestamp => quote!{ ::chrono::offset::Utc.timestamp_opt((value / 1_000_000) as i64, ((value % 1_000_000) * 1000) as u32)
                .single()
                .map(|dt| dt.to_string())
                .unwrap_or("".to_string()).into()
            },
            TantivyType::JsonObject => quote!{ format!("{:?}", value).into() },
        };
        let field_ident = Ident::new(field, Span::call_site());
        quote! {
            if let ::std::option::Option::Some(ref value) = document.#field_ident {
                hashmap.insert(#field, #accessor);
            }
        }
    }).collect();

    let expanded = quote! {
        impl<'a> ::std::convert::From<&'a #name> for ::std::collections::HashMap<&'static str, ::std::borrow::Cow<'a, str>> {
            fn from(document: &'a #name) -> Self {
                use ::chrono::offset::TimeZone;
                let mut hashmap = ::std::collections::HashMap::new();
                #( #to_display_stmts )*
                hashmap
            }
        }

        impl #impl_generics crate::schema::YaffleSchema for #name #ty_generics #where_clause {
            fn from_gelf(gelf_msg: &crate::gelf::GELFMessage) -> ::std::result::Result<#name, ::std::boxed::Box<dyn ::std::error::Error>> {
                use ::std::convert::TryInto;
                Ok(#name {
                    #( #gelf_construct_exprs ),*
                })
            }

            fn from_syslog(syslog_msg: &crate::syslog::SyslogMessage) -> ::std::result::Result<#name, ::std::boxed::Box<dyn ::std::error::Error>> {
                Ok(#name {
                    #( #syslog_construct_exprs ),*
                })
            }

            fn tantivy_schema() -> ::tantivy::schema::Schema {
                let mut schema_builder = ::tantivy::schema::SchemaBuilder::default();
                #( #schema_builder_stmts; )*
                schema_builder.build()
            }

            fn quickwit_mapping() -> std::vec::Vec<crate::quickwit::FieldMapping> {
                vec![
                    #( #quickwit_document_exprs, )*
                ]
            }
        }
    };

    // Hand the output tokens back to the compiler
    expanded.into()
}
