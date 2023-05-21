use crate::schema::Document;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Debug)]
pub(crate) enum Tokenizer {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "raw")]
    Raw,
    #[serde(rename = "en_stem")]
    EnglishStem,
    #[serde(rename = "chinese_compatible")]
    ChineseCompatible,
}

#[derive(Serialize, Debug)]
pub(crate) enum RecordOption {
    #[serde(rename = "basic")]
    Basic,
    #[serde(rename = "freq")]
    Frequency,
    #[serde(rename = "position")]
    Position,
}

#[derive(Serialize, Debug)]
pub(crate) enum DatePrecision {
    #[serde(rename = "seconds")]
    Seconds,
    #[serde(rename = "milliseconds")]
    Milliseconds,
    #[serde(rename = "microseconds")]
    Microseconds,
}

#[derive(Serialize, Debug)]
pub(crate) enum DateOutputFormat {
    #[serde(rename = "unix_timestamp_secs")]
    UnixTimestampSecs,
    #[serde(rename = "unix_timestamp_millis")]
    UnixTimestampMillis,
    #[serde(rename = "unix_timestamp_micros")]
    UnixTimestampMicros,
}

#[derive(Serialize, Debug)]
pub(crate) struct FieldMapping {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) type_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "Clone::clone")] // Skip if true (the default val)
    pub(crate) stored: bool,
    #[serde(skip_serializing_if = "Clone::clone")] // Skip if true (the default val)
    pub(crate) indexed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tokenizer: Option<Tokenizer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) record: Option<RecordOption>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) fieldnorms: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) fast: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) input_formats: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output_format: Option<DateOutputFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) precision: Option<DatePrecision>,
}

impl Default for FieldMapping {
    fn default() -> Self {
        Self {
            name: "".to_string(),
            type_: "".to_string(),
            description: None,
            stored: true,
            indexed: true,
            tokenizer: None,
            record: None,
            fieldnorms: false,
            fast: false,
            input_formats: Vec::default(),
            output_format: None,
            precision: None,
        }
    }
}

#[derive(Serialize, Debug)]
pub(crate) enum MappingMode {
    #[serde(rename = "strict")]
    Strict,
    #[serde(rename = "lenient")]
    Lenient,
    #[serde(rename = "dynamic")]
    Dynamic,
}

#[derive(Serialize, Debug)]
pub(crate) struct DocumentMapping {
    pub(crate) field_mappings: Vec<FieldMapping>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mode: Option<MappingMode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tag_fields: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) store_source: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) timestamp_field: String,
}

#[derive(Serialize, Debug)]
pub(crate) struct RetentionSettings {
    pub(crate) period: String,
    pub(crate) schedule: String,
}

#[derive(Serialize, Debug)]
pub(crate) struct SearchSettings {
    pub(crate) default_search_fields: Vec<String>,
}

#[derive(Serialize, Debug)]
pub(crate) struct IndexCreateRequest {
    pub(crate) doc_mapping: DocumentMapping,
    pub(crate) index_id: String,
    pub(crate) retention: RetentionSettings,
    pub(crate) search_settings: SearchSettings,
    pub(crate) version: String,
}

#[derive(Deserialize, Debug)]
pub(crate) struct SearchResults {
    pub(crate) elapsed_time_micros: u64,
    pub(crate) errors: Vec<String>,
    pub(crate) hits: Vec<Document>,
    pub(crate) num_hits: u64,
}
