// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::path::Path;

use itertools::Itertools;
use prost_reflect::{
    Cardinality, DescriptorPool, DynamicMessage, FieldDescriptor, Kind, MessageDescriptor,
    ReflectMessage, Value,
};
use risingwave_common::array::{ListValue, StructValue};
use risingwave_common::error::ErrorCode::{InternalError, ProtocolError};
use risingwave_common::error::{Result, RwError};
use risingwave_common::try_match_expand;
use risingwave_common::types::{DataType, Datum, Decimal, ScalarImpl, F32, F64};
use risingwave_pb::plan_common::ColumnDesc;
use url::Url;

use super::schema_resolver::*;
use crate::aws_utils::load_file_descriptor_from_s3;
use crate::parser::schema_registry::{extract_schema_id, get_subject_by_strategy, Client};
use crate::parser::unified::protobuf::ProtobufAccess;
use crate::parser::unified::AccessImpl;
use crate::parser::{AccessBuilder, EncodingProperties};

#[derive(Debug)]
pub struct ProtobufAccessBuilder {
    confluent_wire_type: bool,
    message_descriptor: MessageDescriptor,
}

impl AccessBuilder for ProtobufAccessBuilder {
    #[allow(clippy::unused_async)]
    async fn generate_accessor(&mut self, payload: Vec<u8>) -> Result<AccessImpl<'_, '_>> {
        let payload = if self.confluent_wire_type {
            resolve_pb_header(&payload)?
        } else {
            &payload
        };

        let message = DynamicMessage::decode(self.message_descriptor.clone(), payload)
            .map_err(|e| ProtocolError(format!("parse message failed: {}", e)))?;

        Ok(AccessImpl::Protobuf(ProtobufAccess::new(message)))
    }
}

impl ProtobufAccessBuilder {
    pub fn new(config: ProtobufParserConfig) -> Result<Self> {
        let ProtobufParserConfig {
            confluent_wire_type,
            message_descriptor,
        } = config;
        Ok(Self {
            confluent_wire_type,
            message_descriptor,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ProtobufParserConfig {
    confluent_wire_type: bool,
    message_descriptor: MessageDescriptor,
}

impl ProtobufParserConfig {
    pub async fn new(encoding_properties: EncodingProperties) -> Result<Self> {
        let protobuf_config = try_match_expand!(encoding_properties, EncodingProperties::Protobuf)?;
        let location = &protobuf_config.row_schema_location;
        let message_name = &protobuf_config.message_name;
        let url = Url::parse(location)
            .map_err(|e| InternalError(format!("failed to parse url ({}): {}", location, e)))?;

        let schema_bytes = if protobuf_config.use_schema_registry {
            let (schema_key, schema_value) = get_subject_by_strategy(
                &protobuf_config.name_strategy,
                protobuf_config.topic.as_str(),
                protobuf_config.key_message_name.as_deref(),
                Some(message_name.as_ref()),
                protobuf_config.enable_upsert,
            )?;
            tracing::debug!(
                "infer key subject {}, value subject {}",
                schema_key,
                schema_value,
            );
            let client = Client::new(url, &protobuf_config.client_config)?;
            compile_file_descriptor_from_schema_registry(schema_value.as_str(), &client).await?
        } else {
            match url.scheme() {
                // TODO(Tao): support local file only when it's compiled in debug mode.
                "file" => {
                    let path = url.to_file_path().map_err(|_| {
                        RwError::from(InternalError(format!("illegal path: {}", location)))
                    })?;

                    if path.is_dir() {
                        return Err(RwError::from(ProtocolError(
                            "schema file location must not be a directory".to_string(),
                        )));
                    }
                    Self::local_read_to_bytes(&path)
                }
                "s3" => {
                    load_file_descriptor_from_s3(
                        &url,
                        protobuf_config.aws_auth_props.as_ref().unwrap(),
                    )
                    .await
                }
                "https" | "http" => load_file_descriptor_from_http(&url).await,
                scheme => Err(RwError::from(ProtocolError(format!(
                    "path scheme {} is not supported",
                    scheme
                )))),
            }?
        };

        let pool = DescriptorPool::decode(schema_bytes.as_slice()).map_err(|e| {
            ProtocolError(format!(
                "cannot build descriptor pool from schema: {}, error: {}",
                location, e
            ))
        })?;
        let message_descriptor = pool.get_message_by_name(message_name).ok_or_else(|| {
            ProtocolError(format!(
                "cannot find message {} in schema: {}.\n poll is {:?}",
                message_name, location, pool
            ))
        })?;
        Ok(Self {
            message_descriptor,
            confluent_wire_type: protobuf_config.use_schema_registry,
        })
    }

    /// read binary schema from a local file
    fn local_read_to_bytes(path: &Path) -> Result<Vec<u8>> {
        std::fs::read(path).map_err(|e| {
            RwError::from(InternalError(format!(
                "failed to read file {}: {}",
                path.display(),
                e
            )))
        })
    }

    /// Maps the protobuf schema to relational schema.
    pub fn map_to_columns(&self) -> Result<Vec<ColumnDesc>> {
        let mut columns = Vec::with_capacity(self.message_descriptor.fields().len());
        let mut index = 0;
        let mut parse_trace: Vec<String> = vec![];
        for field in self.message_descriptor.fields() {
            columns.push(Self::pb_field_to_col_desc(
                &field,
                &mut index,
                &mut parse_trace,
            )?);
        }

        Ok(columns)
    }

    /// Maps a protobuf field to a RW column.
    fn pb_field_to_col_desc(
        field_descriptor: &FieldDescriptor,
        index: &mut i32,
        parse_trace: &mut Vec<String>,
    ) -> Result<ColumnDesc> {
        let field_type = protobuf_type_mapping(field_descriptor, parse_trace)?;
        if let Kind::Message(m) = field_descriptor.kind() {
            let field_descs = if let DataType::List { .. } = field_type {
                vec![]
            } else {
                m.fields()
                    .map(|f| Self::pb_field_to_col_desc(&f, index, parse_trace))
                    .collect::<Result<Vec<_>>>()?
            };
            *index += 1;
            Ok(ColumnDesc {
                column_id: *index,
                name: field_descriptor.name().to_string(),
                column_type: Some(field_type.to_protobuf()),
                field_descs,
                type_name: m.full_name().to_string(),
                generated_or_default_column: None,
            })
        } else {
            *index += 1;
            Ok(ColumnDesc {
                column_id: *index,
                name: field_descriptor.name().to_string(),
                column_type: Some(field_type.to_protobuf()),
                ..Default::default()
            })
        }
    }
}

fn detect_loop_and_push(trace: &mut Vec<String>, fd: &FieldDescriptor) -> Result<()> {
    let identifier = format!("{}({})", fd.name(), fd.full_name());
    if trace.iter().any(|s| s == identifier.as_str()) {
        return Err(RwError::from(ProtocolError(format!(
            "circular reference detected: {}, conflict with {}, kind {:?}",
            trace.iter().join("->"),
            identifier,
            fd.kind(),
        ))));
    }
    trace.push(identifier);
    Ok(())
}

pub fn from_protobuf_value(field_desc: &FieldDescriptor, value: &Value) -> Result<Datum> {
    let v = match value {
        Value::Bool(v) => ScalarImpl::Bool(*v),
        Value::I32(i) => ScalarImpl::Int32(*i),
        Value::U32(i) => ScalarImpl::Int64(*i as i64),
        Value::I64(i) => ScalarImpl::Int64(*i),
        Value::U64(i) => ScalarImpl::Decimal(Decimal::from(*i)),
        Value::F32(f) => ScalarImpl::Float32(F32::from(*f)),
        Value::F64(f) => ScalarImpl::Float64(F64::from(*f)),
        Value::String(s) => ScalarImpl::Utf8(s.as_str().into()),
        Value::EnumNumber(idx) => {
            let kind = field_desc.kind();
            let enum_desc = kind.as_enum().ok_or_else(|| {
                let err_msg = format!("protobuf parse error.not a enum desc {:?}", field_desc);
                RwError::from(ProtocolError(err_msg))
            })?;
            let enum_symbol = enum_desc.get_value(*idx).ok_or_else(|| {
                let err_msg = format!(
                    "protobuf parse error.unknown enum index {} of enum {:?}",
                    idx, enum_desc
                );
                RwError::from(ProtocolError(err_msg))
            })?;
            ScalarImpl::Utf8(enum_symbol.name().into())
        }
        Value::Message(dyn_msg) => {
            let mut rw_values = Vec::with_capacity(dyn_msg.descriptor().fields().len());
            // fields is a btree map in descriptor
            // so it's order is the same as datatype
            for field_desc in dyn_msg.descriptor().fields() {
                // missing field
                if !dyn_msg.has_field(&field_desc)
                    && field_desc.cardinality() == Cardinality::Required
                {
                    let err_msg = format!(
                        "protobuf parse error.missing required field {:?}",
                        field_desc
                    );
                    return Err(RwError::from(ProtocolError(err_msg)));
                }
                // use default value if dyn_msg doesn't has this field
                let value = dyn_msg.get_field(&field_desc);
                rw_values.push(from_protobuf_value(&field_desc, &value)?);
            }
            ScalarImpl::Struct(StructValue::new(rw_values))
        }
        Value::List(values) => {
            let rw_values = values
                .iter()
                .map(|value| from_protobuf_value(field_desc, value))
                .collect::<Result<Vec<_>>>()?;
            ScalarImpl::List(ListValue::new(rw_values))
        }
        Value::Bytes(value) => ScalarImpl::Bytea(value.to_vec().into_boxed_slice()),
        _ => {
            let err_msg = format!(
                "protobuf parse error.unsupported type {:?}, value {:?}",
                field_desc, value
            );
            return Err(RwError::from(InternalError(err_msg)));
        }
    };
    Ok(Some(v))
}

/// Maps protobuf type to RW type.
fn protobuf_type_mapping(
    field_descriptor: &FieldDescriptor,
    parse_trace: &mut Vec<String>,
) -> Result<DataType> {
    detect_loop_and_push(parse_trace, field_descriptor)?;
    let field_type = field_descriptor.kind();
    let mut t = match field_type {
        Kind::Bool => DataType::Boolean,
        Kind::Double => DataType::Float64,
        Kind::Float => DataType::Float32,
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 | Kind::Fixed32 => DataType::Int32,
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 | Kind::Fixed64 | Kind::Uint32 => {
            DataType::Int64
        }
        Kind::Uint64 => DataType::Decimal,
        Kind::String => DataType::Varchar,
        Kind::Message(m) => {
            let fields = m
                .fields()
                .map(|f| protobuf_type_mapping(&f, parse_trace))
                .collect::<Result<Vec<_>>>()?;
            let field_names = m.fields().map(|f| f.name().to_string()).collect_vec();
            DataType::new_struct(fields, field_names)
        }
        Kind::Enum(_) => DataType::Varchar,
        Kind::Bytes => DataType::Bytea,
    };
    if field_descriptor.cardinality() == Cardinality::Repeated {
        t = DataType::List(Box::new(t))
    }
    _ = parse_trace.pop();
    Ok(t)
}

pub(crate) fn resolve_pb_header(payload: &[u8]) -> Result<&[u8]> {
    // there's a message index array at the front of payload
    // if it is the first message in proto def, the array is just and `0`
    // TODO: support parsing more complex indec array
    let (_, remained) = extract_schema_id(payload)?;
    match remained.first() {
        Some(0) => Ok(&remained[1..]),
        Some(i) => {
            Err(RwError::from(ProtocolError(format!("The payload message must be the first message in protobuf schema def, but the message index is {}", i))))
        }
        None => {
            Err(RwError::from(ProtocolError("The proto payload is empty".to_owned())))
        }
    }
}

#[cfg(test)]
mod test {

    use std::collections::HashMap;
    use std::path::PathBuf;

    use bytes::Bytes;
    use risingwave_pb::catalog::StreamSourceInfo;
    use risingwave_pb::data::data_type::PbTypeName;

    use super::*;
    use crate::parser::SpecificParserConfig;
    use crate::source::{SourceEncode, SourceFormat, SourceStruct};

    fn schema_dir() -> String {
        let dir = PathBuf::from("src/test_data");
        format!(
            "file://{}",
            std::fs::canonicalize(dir).unwrap().to_str().unwrap()
        )
    }

    // Id:      123,
    // Address: "test address",
    // City:    "test city",
    // Zipcode: 456,
    // Rate:    1.2345,
    // Date:    "2021-01-01"
    static PRE_GEN_PROTO_DATA: &[u8] = b"\x08\x7b\x12\x0c\x74\x65\x73\x74\x20\x61\x64\x64\x72\x65\x73\x73\x1a\x09\x74\x65\x73\x74\x20\x63\x69\x74\x79\x20\xc8\x03\x2d\x19\x04\x9e\x3f\x32\x0a\x32\x30\x32\x31\x2d\x30\x31\x2d\x30\x31";

    #[tokio::test]
    async fn test_simple_schema() -> Result<()> {
        let location = schema_dir() + "/simple-schema";
        println!("location: {}", location);
        let message_name = "test.TestRecord";
        let info = StreamSourceInfo {
            proto_message_name: message_name.to_string(),
            row_schema_location: location.to_string(),
            use_schema_registry: false,
            ..Default::default()
        };
        let parser_config = SpecificParserConfig::new(
            SourceStruct::new(SourceFormat::Plain, SourceEncode::Protobuf),
            &info,
            &HashMap::new(),
        )?;
        let conf = ProtobufParserConfig::new(parser_config.encoding_config).await?;
        let value = DynamicMessage::decode(conf.message_descriptor, PRE_GEN_PROTO_DATA).unwrap();

        assert_eq!(
            value.get_field_by_name("id").unwrap().into_owned(),
            Value::I32(123)
        );
        assert_eq!(
            value.get_field_by_name("address").unwrap().into_owned(),
            Value::String("test address".to_string())
        );
        assert_eq!(
            value.get_field_by_name("city").unwrap().into_owned(),
            Value::String("test city".to_string())
        );
        assert_eq!(
            value.get_field_by_name("zipcode").unwrap().into_owned(),
            Value::I64(456)
        );
        assert_eq!(
            value.get_field_by_name("rate").unwrap().into_owned(),
            Value::F32(1.2345)
        );
        assert_eq!(
            value.get_field_by_name("date").unwrap().into_owned(),
            Value::String("2021-01-01".to_string())
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_complex_schema() -> Result<()> {
        let location = schema_dir() + "/complex-schema";
        let message_name = "test.User";

        let info = StreamSourceInfo {
            proto_message_name: message_name.to_string(),
            row_schema_location: location.to_string(),
            use_schema_registry: false,
            ..Default::default()
        };
        let parser_config = SpecificParserConfig::new(
            SourceStruct::new(SourceFormat::Plain, SourceEncode::Protobuf),
            &info,
            &HashMap::new(),
        )?;
        let conf = ProtobufParserConfig::new(parser_config.encoding_config).await?;
        let columns = conf.map_to_columns().unwrap();

        assert_eq!(columns[0].name, "id".to_string());
        assert_eq!(columns[1].name, "code".to_string());
        assert_eq!(columns[2].name, "timestamp".to_string());

        let data_type = columns[3].column_type.as_ref().unwrap();
        assert_eq!(data_type.get_type_name().unwrap(), PbTypeName::List);
        let inner_field_type = data_type.field_type.clone();
        assert_eq!(
            inner_field_type[0].get_type_name().unwrap(),
            PbTypeName::Struct
        );
        let struct_inner = inner_field_type[0].field_type.clone();
        assert_eq!(struct_inner[0].get_type_name().unwrap(), PbTypeName::Int32);
        assert_eq!(struct_inner[1].get_type_name().unwrap(), PbTypeName::Int32);
        assert_eq!(
            struct_inner[2].get_type_name().unwrap(),
            PbTypeName::Varchar
        );

        assert_eq!(columns[4].name, "contacts".to_string());
        let inner_field_type = columns[4].column_type.as_ref().unwrap().field_type.clone();
        assert_eq!(
            inner_field_type[0].get_type_name().unwrap(),
            PbTypeName::List
        );
        assert_eq!(
            inner_field_type[1].get_type_name().unwrap(),
            PbTypeName::List
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_refuse_recursive_proto_message() {
        let location = schema_dir() + "/proto_recursive/recursive.pb";
        let message_name = "recursive.ComplexRecursiveMessage";

        let info = StreamSourceInfo {
            proto_message_name: message_name.to_string(),
            row_schema_location: location.to_string(),
            use_schema_registry: false,
            ..Default::default()
        };
        let parser_config = SpecificParserConfig::new(
            SourceStruct::new(SourceFormat::Plain, SourceEncode::Protobuf),
            &info,
            &HashMap::new(),
        )
        .unwrap();
        let conf = ProtobufParserConfig::new(parser_config.encoding_config)
            .await
            .unwrap();
        let columns = conf.map_to_columns();
        // expect error message:
        // "Err(Protocol error: circular reference detected:
        // parent(recursive.ComplexRecursiveMessage.parent)->siblings(recursive.
        // ComplexRecursiveMessage.Parent.siblings), conflict with
        // parent(recursive.ComplexRecursiveMessage.parent), kind
        // recursive.ComplexRecursiveMessage.Parent"
        assert!(columns.is_err());
    }

    #[tokio::test]
    async fn test_all_types() {
        let location = schema_dir() + "/proto_recursive/recursive.pb";
        let message_name = "recursive.AllTypes";

        let info = StreamSourceInfo {
            proto_message_name: message_name.to_string(),
            row_schema_location: location.to_string(),
            use_schema_registry: false,
            ..Default::default()
        };
        let parser_config = SpecificParserConfig::new(
            SourceStruct::new(SourceFormat::Plain, SourceEncode::Protobuf),
            &info,
            &HashMap::new(),
        )
        .unwrap();
        let conf = ProtobufParserConfig::new(parser_config.encoding_config)
            .await
            .unwrap();
        // Ensure that the parser can recognize the schema.
        conf.map_to_columns().unwrap();

        let field_desc = conf
            .message_descriptor
            .get_field_by_name("bytes_field")
            .unwrap();
        let d = from_protobuf_value(&field_desc, &Value::Bytes(Bytes::from(vec![1, 2, 3])))
            .unwrap()
            .unwrap();
        assert_eq!(d, ScalarImpl::Bytea(vec![1, 2, 3].into_boxed_slice()));
    }
}
