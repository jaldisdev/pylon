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

//! Walks a compiled query's `ShapeNode` alongside its decoded `DecodedValue`
//! result to build a generic [`Value`](crate::value::Value) — the Rust
//! port of `pylon/query.py`'s `_decode`/`_decode_json_tuple`/
//! `_decode_json_member` (there's no per-type dataclass registry to hydrate
//! against here, so every object-shaped result becomes a generic
//! [`Object`](crate::value::Object) instead).

use pylon_core::query::{JsonMember, JsonMemberKind, ShapeNode};
use pylon_value::DecodedValue;

use crate::value::{Group, Object, Range, Value};

/// Decode `value` (the query's own `result` column, already wire-decoded
/// into a `DecodedValue`) using `shape` (the compiled query's own output
/// shape) into the crate's generic [`Value`].
pub fn decode(shape: &ShapeNode, value: &DecodedValue) -> Value {
    match shape {
        // The row is the root object's own tuple.
        ShapeNode::Object { position: 0, .. } => decode_inner(shape, value, Some(0)),
        _ => decode_inner(shape, value, None),
    }
}

/// `position_override` is `Some(0)` when `value` has already been extracted
/// from its enclosing composite (an array/group element, or a vector/FTS
/// search's object sub-tuple) — mirrors `pylon/query.py`'s `{**node,
/// "position": 0}` overrides in `_decode`'s `"array"`/`"group"`/
/// `"vector_search"`/`"fts_search"` branches.
fn decode_inner(shape: &ShapeNode, value: &DecodedValue, position_override: Option<usize>) -> Value {
    match shape {
        ShapeNode::Scalar { position, .. } => {
            cached_to_value(&composite_at(value, position_override.unwrap_or(*position)))
        }
        ShapeNode::RawScalar | ShapeNode::JsonScalar => cached_to_value(value),
        ShapeNode::Object {
            type_name,
            position,
            pointers,
            ..
        } => {
            // An object at position 0 of the enclosing tuple is a free
            // object's first field, not the tuple itself.
            let obj_tuple = match position_override {
                Some(_) => value.clone(),
                None => composite_at(value, *position),
            };
            decode_object(&obj_tuple, type_name.as_deref(), pointers)
        }
        ShapeNode::Array { position, element, .. } => {
            decode_array(value, position_override.unwrap_or(*position), element)
        }
        ShapeNode::NamedTuple {
            position,
            type_name,
            members,
            ..
        } => decode_named_tuple(
            value,
            position_override.unwrap_or(*position),
            type_name.as_deref(),
            members.as_deref(),
        ),
        ShapeNode::Enum {
            position, enum_type, ..
        } => decode_enum(value, position_override.unwrap_or(*position), enum_type),
        ShapeNode::Tuple { elements, .. } => {
            Value::Tuple(elements.iter().map(|e| decode_inner(e, value, None)).collect())
        }
        ShapeNode::Group {
            key_nodes,
            grouping_position,
            elements_position,
            element,
        } => decode_group(value, key_nodes, *grouping_position, *elements_position, element),
        ShapeNode::VectorSearch {
            object_position,
            distance_position,
            object_node,
        } => decode_vector_search(value, *object_position, *distance_position, object_node),
        ShapeNode::FtsSearch {
            object_position,
            rank_position,
            object_node,
        } => decode_fts_search(value, *object_position, *rank_position, object_node),
    }
}

/// Extracts the field at `pos` from a composite (tuple/`ROW(...)`) value —
/// the Rust equivalent of Python's `value[pos]` on a record row.
/// Anything else (a bare scalar reached with `pos == 0`, or a genuinely
/// out-of-range position) degrades to `Null` rather than panicking — the
/// shape and the SQL that produced `value` are always built together by
/// the same compiler pass, so a mismatch here would be an internal
/// compiler bug, not a case the caller needs to recover from gracefully.
fn composite_at(value: &DecodedValue, pos: usize) -> DecodedValue {
    match value {
        DecodedValue::Composite(fields) => fields.get(pos).cloned().unwrap_or(DecodedValue::Null),
        _ => DecodedValue::Null,
    }
}

/// A pointer's own output name — only `Scalar`/`Enum`/`NamedTuple`/
/// `Object`/`Array` ever appear inside an `Object`'s own `pointers` list
/// (mirrors `pylon-py`'s `shape_node_to_py`, where `RawScalar`/`JsonScalar`
/// carry no name/position at all and so can only ever be a shape's root).
fn pointer_name(node: &ShapeNode) -> &str {
    match node {
        ShapeNode::Scalar { name, .. }
        | ShapeNode::Enum { name, .. }
        | ShapeNode::NamedTuple { name, .. }
        | ShapeNode::Object { name, .. }
        | ShapeNode::Array { name, .. } => name,
        other => unreachable!("shape node kind never appears as an object's own pointer: {other:?}"),
    }
}

fn pointer_position(node: &ShapeNode) -> usize {
    match node {
        ShapeNode::Scalar { position, .. }
        | ShapeNode::Enum { position, .. }
        | ShapeNode::NamedTuple { position, .. }
        | ShapeNode::Object { position, .. }
        | ShapeNode::Array { position, .. } => *position,
        other => unreachable!("shape node kind never appears as an object's own pointer: {other:?}"),
    }
}

/// `ShapeNode::Enum.enum_type` carries the Postgres-schema-qualified form
/// (e.g. `public::Gender`) — only the `default` module is ever renamed (to
/// Postgres `public`), so that's the only translation to undo, mirroring
/// `pylon/query.py`'s `_pylon_qualify_enum_type`/`_pg_schema_to_pylon_module`.
fn pg_schema_qualified_to_pylon(qualified: &str) -> String {
    match qualified.split_once("::") {
        Some(("public", rest)) => format!("default::{rest}"),
        _ => qualified.to_string(),
    }
}

fn decode_object(obj_tuple: &DecodedValue, type_name: Option<&str>, pointers: &[ShapeNode]) -> Value {
    if matches!(obj_tuple, DecodedValue::Null) {
        return Value::Null;
    }
    // pointers[0] is always the auto-injected __type__ discriminator
    // (position 0); skip it — an explicit __type__ the user asked for
    // appears at position > 0 and is included like any other field.
    let mut fields: Vec<(String, Value)> = Vec::new();
    for p in pointers
        .iter()
        .filter(|p| !(pointer_name(p) == "__type__" && pointer_position(p) == 0))
    {
        let value = decode_inner(p, obj_tuple, None);
        // A name repeats only where splats overlap (`*` beside
        // `[is Sub].*`, or two intersections), and an intersection the row
        // is not of reads nothing: the value it does have wins.
        match fields.iter_mut().find(|(name, _)| name == pointer_name(p)) {
            Some((_, existing)) if is_nothing(existing) => *existing = value,
            Some(_) => {}
            None => fields.push((pointer_name(p).to_string(), value)),
        }
    }
    // Use the actual per-row __type__ value for the reported type name —
    // for a polymorphic (interface) query this is the real concrete type,
    // not the interface's own static type_name.
    let resolved_type_name = type_name.map(|static_name| match composite_at(obj_tuple, 0) {
        DecodedValue::Str(s) if !s.is_empty() => s,
        _ => static_name.to_string(),
    });
    Value::Object(Object {
        type_name: resolved_type_name,
        fields,
    })
}

fn is_nothing(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

fn decode_array(value: &DecodedValue, position: usize, element: &ShapeNode) -> Value {
    let items = match composite_at(value, position) {
        DecodedValue::Array(items) => items,
        _ => vec![],
    };
    Value::Array(items.iter().map(|item| decode_inner(element, item, Some(0))).collect())
}

fn decode_named_tuple(
    value: &DecodedValue,
    position: usize,
    type_name: Option<&str>,
    members: Option<&[JsonMember]>,
) -> Value {
    // Root-level named tuples arrive as the raw decoded jsonb value
    // directly (Object for named members, Array for positional/unnamed
    // ones, or Null); a nested one sits at a positional index inside the
    // parent composite row.
    let raw = match value {
        DecodedValue::Object(_) | DecodedValue::Array(_) | DecodedValue::Null => value.clone(),
        _ => composite_at(value, position),
    };
    decode_json_tuple(&raw, type_name, members)
}

/// Decodes a jsonb tuple/named-tuple value using its statically-known
/// member shape — a `Value::Tuple` for positional members, a `Value::Object`
/// for named members (nominal or structural — this generic client has no
/// registry to hydrate a nominal type against, so both decode the same
/// way, `type_name` carried along only as metadata). Falls back to a
/// generic structural conversion when no member shape was known at compile
/// time.
fn decode_json_tuple(value: &DecodedValue, type_name: Option<&str>, members: Option<&[JsonMember]>) -> Value {
    if matches!(value, DecodedValue::Null) {
        return Value::Null;
    }
    let Some(members) = members else {
        return cached_to_value(value);
    };
    let positional = members.iter().all(|m| m.key.is_none());
    if positional {
        let items: &[DecodedValue] = match value {
            DecodedValue::Array(a) => a.as_slice(),
            _ => &[],
        };
        let elements = members
            .iter()
            .enumerate()
            .map(|(i, m)| decode_json_member(items.get(i).unwrap_or(&DecodedValue::Null), m))
            .collect();
        return Value::Tuple(elements);
    }
    let obj_fields: &[(String, DecodedValue)] = match value {
        DecodedValue::Object(o) => o.as_slice(),
        _ => &[],
    };
    let fields = members
        .iter()
        .map(|m| {
            let key = m.key.clone().expect("named branch: every member has a key");
            let raw = obj_fields
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v)
                .unwrap_or(&DecodedValue::Null);
            (key, decode_json_member(raw, m))
        })
        .collect();
    Value::Object(Object {
        type_name: type_name.map(str::to_string),
        fields,
    })
}

fn decode_json_member(value: &DecodedValue, member: &JsonMember) -> Value {
    match &member.kind {
        JsonMemberKind::Scalar => cached_to_value(value),
        JsonMemberKind::Enum { enum_type } => match value {
            DecodedValue::Null => Value::Null,
            DecodedValue::Str(s) => Value::Enum {
                type_name: pg_schema_qualified_to_pylon(enum_type),
                value: s.clone(),
            },
            other => cached_to_value(other),
        },
        JsonMemberKind::Tuple { type_name, members } => decode_json_tuple(value, type_name.as_deref(), Some(members)),
    }
}

fn decode_enum(value: &DecodedValue, position: usize, enum_type: &str) -> Value {
    // An enum inside an array arrives as the label itself rather than as a
    // field of a record -- the same distinction `decode_named_tuple` draws.
    let raw = match value {
        DecodedValue::Composite(_) => composite_at(value, position),
        other => other.clone(),
    };
    match raw {
        DecodedValue::Null => Value::Null,
        DecodedValue::Str(s) => Value::Enum {
            type_name: pg_schema_qualified_to_pylon(enum_type),
            value: s,
        },
        other => cached_to_value(&other),
    }
}

fn decode_group(
    value: &DecodedValue,
    key_nodes: &[ShapeNode],
    grouping_position: usize,
    elements_position: usize,
    element: &ShapeNode,
) -> Value {
    let key_fields: Vec<(String, Value)> = key_nodes
        .iter()
        .map(|kn| (pointer_name(kn).to_string(), decode_inner(kn, value, None)))
        .collect();
    let key = Object {
        type_name: None,
        fields: key_fields,
    };

    let grouping = match composite_at(value, grouping_position) {
        DecodedValue::Array(items) => items
            .into_iter()
            .filter_map(|v| match v {
                DecodedValue::Str(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => vec![],
    };

    let elements = match composite_at(value, elements_position) {
        DecodedValue::Array(items) => items.iter().map(|item| decode_inner(element, item, Some(0))).collect(),
        _ => vec![],
    };

    Value::Group(Box::new(Group {
        key,
        grouping,
        elements,
    }))
}

fn decode_vector_search(
    value: &DecodedValue,
    object_position: usize,
    distance_position: usize,
    object_node: &ShapeNode,
) -> Value {
    let obj_tuple = composite_at(value, object_position);
    let distance = as_f64(&composite_at(value, distance_position));
    let object = decode_inner(object_node, &obj_tuple, Some(0));
    Value::VectorSearch {
        object: Box::new(object),
        distance,
    }
}

fn decode_fts_search(
    value: &DecodedValue,
    object_position: usize,
    rank_position: usize,
    object_node: &ShapeNode,
) -> Value {
    let obj_tuple = composite_at(value, object_position);
    let score = as_f64(&composite_at(value, rank_position));
    let object = decode_inner(object_node, &obj_tuple, Some(0));
    Value::FtsSearch {
        object: Box::new(object),
        score,
    }
}

fn as_f64(value: &DecodedValue) -> f64 {
    match value {
        DecodedValue::F64(f) => *f,
        DecodedValue::I64(i) => *i as f64,
        _ => 0.0,
    }
}

/// Generic structural conversion with no shape metadata to guide it — used
/// for `RawScalar`/`JsonScalar` leaves (the value's own native decoded type
/// is already correct) and as the fallback when no member shape was known
/// at compile time.
pub(crate) fn cached_to_value(value: &DecodedValue) -> Value {
    match value {
        DecodedValue::Null => Value::Null,
        DecodedValue::Bool(b) => Value::Bool(*b),
        DecodedValue::I64(i) => Value::Int64(*i),
        DecodedValue::F64(f) => Value::Float64(*f),
        DecodedValue::Str(s) => Value::Str(s.clone()),
        DecodedValue::Bytes(b) => Value::Bytes(b.clone()),
        DecodedValue::Uuid(bytes) => Value::Uuid(uuid::Uuid::from_bytes(*bytes)),
        DecodedValue::Decimal(s) => Value::Decimal(s.clone()),
        DecodedValue::Interval {
            months,
            days,
            microseconds,
        } => Value::Duration {
            months: *months,
            days: *days,
            microseconds: *microseconds,
        },
        DecodedValue::Date(d) => Value::Date(*d),
        DecodedValue::Time(t) => Value::Time(*t),
        DecodedValue::Timestamp(t) => Value::Timestamp(*t),
        DecodedValue::Timestamptz(t) => Value::Timestamptz(*t),
        DecodedValue::Array(items) => Value::Array(items.iter().map(cached_to_value).collect()),
        DecodedValue::Composite(items) => Value::Tuple(items.iter().map(cached_to_value).collect()),
        DecodedValue::Object(fields) => Value::Object(Object {
            type_name: None,
            fields: fields.iter().map(|(k, v)| (k.clone(), cached_to_value(v))).collect(),
        }),
        DecodedValue::Range {
            lower,
            upper,
            inc_lower,
            inc_upper,
            empty,
        } => Value::Range(Box::new(Range {
            lower: lower.as_ref().map(|b| cached_to_value(b)),
            upper: upper.as_ref().map(|b| cached_to_value(b)),
            inc_lower: *inc_lower,
            inc_upper: *inc_upper,
            empty: *empty,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pylon_core::query::Cardinality;

    fn comp(items: Vec<DecodedValue>) -> DecodedValue {
        DecodedValue::Composite(items)
    }

    #[test]
    fn decodes_a_root_object_with_a_scalar_property() {
        // (type_disc, name) — the type discriminator always sits at
        // position 0, matching `build_shape`'s own convention.
        let value = comp(vec![
            DecodedValue::Str("default::Person".into()),
            DecodedValue::Str("Bob".into()),
        ]);
        let shape = ShapeNode::Object {
            name: String::new(),
            type_name: Some("default::Person".into()),
            position: 0,
            cardinality: Cardinality::Required,
            pointers: vec![
                ShapeNode::Scalar {
                    name: "__type__".into(),
                    position: 0,
                },
                ShapeNode::Scalar {
                    name: "name".into(),
                    position: 1,
                },
            ],
        };
        let decoded = decode(&shape, &value);
        let Value::Object(obj) = decoded else {
            panic!("expected Object, got {decoded:?}")
        };
        assert_eq!(obj.type_name(), Some("default::Person"));
        assert_eq!(obj.get("name"), Some(&Value::Str("Bob".into())));
        // The injected __type__ pointer at position 0 must not appear as a
        // field of its own.
        assert_eq!(obj.get("__type__"), None);
        assert_eq!(obj.len(), 1);
    }

    #[test]
    fn a_free_objects_first_field_is_read_at_its_position() {
        // `select { b := (…).latest_data { address } }` — no discriminator in
        // a free object, so its first field sits at position 0.
        let value = comp(vec![comp(vec![
            DecodedValue::Str("default::Address".into()),
            DecodedValue::Str("Main St".into()),
        ])]);
        let shape = ShapeNode::Object {
            name: String::new(),
            type_name: None,
            position: 0,
            cardinality: Cardinality::Many,
            pointers: vec![ShapeNode::Object {
                name: "b".into(),
                type_name: Some("default::Address".into()),
                position: 0,
                cardinality: Cardinality::Optional,
                pointers: vec![
                    ShapeNode::Scalar {
                        name: "__type__".into(),
                        position: 0,
                    },
                    ShapeNode::Scalar {
                        name: "address".into(),
                        position: 1,
                    },
                ],
            }],
        };
        let Value::Object(root) = decode(&shape, &value) else { panic!() };
        let Some(Value::Object(b)) = root.get("b") else { panic!("{root:?}") };
        assert_eq!(b.get("address"), Some(&Value::Str("Main St".into())));
    }

    #[test]
    fn a_name_two_splats_share_keeps_the_value_it_has() {
        // `[is A].*` beside `[is B].*`: the intersection the row is not of
        // reads nothing, whichever comes last.
        let value = comp(vec![
            DecodedValue::Str("default::A".into()),
            DecodedValue::Str("kept".into()),
            DecodedValue::Null,
        ]);
        let shape = ShapeNode::Object {
            name: String::new(),
            type_name: Some("default::A".into()),
            position: 0,
            cardinality: Cardinality::Required,
            pointers: vec![
                ShapeNode::Scalar {
                    name: "__type__".into(),
                    position: 0,
                },
                ShapeNode::Scalar {
                    name: "identifier".into(),
                    position: 1,
                },
                ShapeNode::Scalar {
                    name: "identifier".into(),
                    position: 2,
                },
            ],
        };
        let Value::Object(obj) = decode(&shape, &value) else {
            panic!("expected Object")
        };
        assert_eq!(obj.get("identifier"), Some(&Value::Str("kept".into())));
        assert_eq!(obj.len(), 1);
    }

    #[test]
    fn polymorphic_object_uses_the_actual_row_type_not_the_static_one() {
        let value = comp(vec![DecodedValue::Str("default::Individual".into())]);
        let shape = ShapeNode::Object {
            name: String::new(),
            type_name: Some("default::Account".into()),
            position: 0,
            cardinality: Cardinality::Required,
            pointers: vec![ShapeNode::Scalar {
                name: "__type__".into(),
                position: 0,
            }],
        };
        let Value::Object(obj) = decode(&shape, &value) else {
            panic!("expected Object")
        };
        assert_eq!(obj.type_name(), Some("default::Individual"));
    }

    #[test]
    fn null_object_decodes_to_null() {
        let value = comp(vec![DecodedValue::Null, DecodedValue::Null]);
        let shape = ShapeNode::Object {
            name: "employer".into(),
            type_name: Some("default::Company".into()),
            position: 1,
            cardinality: Cardinality::Optional,
            pointers: vec![],
        };
        assert_eq!(decode(&shape, &value), Value::Null);
    }

    #[test]
    fn decodes_a_nested_array_of_objects() {
        let item = comp(vec![
            DecodedValue::Str("default::Tag".into()),
            DecodedValue::Str("rust".into()),
        ]);
        let value = comp(vec![
            DecodedValue::Str("default::Post".into()),
            DecodedValue::Array(vec![item]),
        ]);
        let shape = ShapeNode::Array {
            name: "tags".into(),
            position: 1,
            element: Box::new(ShapeNode::Object {
                name: String::new(),
                type_name: Some("default::Tag".into()),
                position: 0,
                cardinality: Cardinality::Many,
                pointers: vec![
                    ShapeNode::Scalar {
                        name: "__type__".into(),
                        position: 0,
                    },
                    ShapeNode::Scalar {
                        name: "name".into(),
                        position: 1,
                    },
                ],
            }),
        };
        let Value::Array(items) = decode(&shape, &value) else {
            panic!("expected Array")
        };
        assert_eq!(items.len(), 1);
        let Value::Object(tag) = &items[0] else {
            panic!("expected Object element")
        };
        assert_eq!(tag.get("name"), Some(&Value::Str("rust".into())));
    }

    #[test]
    fn enum_translates_public_schema_back_to_default_module() {
        let value = comp(vec![DecodedValue::Str("Male".into())]);
        let shape = ShapeNode::Enum {
            name: "gender".into(),
            position: 0,
            enum_type: "public::Gender".into(),
        };
        assert_eq!(
            decode(&shape, &value),
            Value::Enum {
                type_name: "default::Gender".into(),
                value: "Male".into()
            },
        );
    }

    #[test]
    fn enum_in_a_non_default_module_is_left_unqualified_untranslated() {
        let value = comp(vec![DecodedValue::Str("Active".into())]);
        let shape = ShapeNode::Enum {
            name: "status".into(),
            position: 0,
            enum_type: "billing::Status".into(),
        };
        assert_eq!(
            decode(&shape, &value),
            Value::Enum {
                type_name: "billing::Status".into(),
                value: "Active".into()
            },
        );
    }

    #[test]
    fn raw_scalar_and_json_scalar_pass_the_value_through_directly() {
        let value = DecodedValue::Array(vec![DecodedValue::I64(1), DecodedValue::I64(2)]);
        assert_eq!(
            decode(&ShapeNode::RawScalar, &value),
            Value::Array(vec![Value::Int64(1), Value::Int64(2)])
        );
        assert_eq!(
            decode(&ShapeNode::JsonScalar, &value),
            Value::Array(vec![Value::Int64(1), Value::Int64(2)])
        );
    }

    #[test]
    fn decodes_a_positional_structural_tuple() {
        let value = comp(vec![DecodedValue::I64(1), DecodedValue::I64(2)]);
        let shape = ShapeNode::Tuple {
            position: 0,
            elements: vec![
                ShapeNode::Scalar {
                    name: String::new(),
                    position: 0,
                },
                ShapeNode::Scalar {
                    name: String::new(),
                    position: 1,
                },
            ],
            names: None,
        };
        assert_eq!(
            decode(&shape, &value),
            Value::Tuple(vec![Value::Int64(1), Value::Int64(2)])
        );
    }

    #[test]
    fn decodes_a_registered_named_tuple_from_jsonb() {
        let raw = DecodedValue::Object(vec![
            ("x".into(), DecodedValue::F64(1.0)),
            ("y".into(), DecodedValue::F64(2.0)),
        ]);
        let value = comp(vec![DecodedValue::Str("default::Person".into()), raw]);
        let shape = ShapeNode::NamedTuple {
            name: "location".into(),
            position: 1,
            type_name: Some("default::Point".into()),
            members: Some(vec![
                JsonMember {
                    key: Some("x".into()),
                    kind: JsonMemberKind::Scalar,
                },
                JsonMember {
                    key: Some("y".into()),
                    kind: JsonMemberKind::Scalar,
                },
            ]),
            is_free_object: false,
        };
        let Value::Object(point) = decode(&shape, &value) else {
            panic!("expected Object")
        };
        assert_eq!(point.type_name(), Some("default::Point"));
        assert_eq!(point.get("x"), Some(&Value::Float64(1.0)));
        assert_eq!(point.get("y"), Some(&Value::Float64(2.0)));
    }

    #[test]
    fn decodes_an_unregistered_structural_named_tuple_member_enum() {
        let raw = DecodedValue::Object(vec![("gender".into(), DecodedValue::Str("Female".into()))]);
        let shape = ShapeNode::NamedTuple {
            name: String::new(),
            position: 0,
            type_name: None,
            members: Some(vec![JsonMember {
                key: Some("gender".into()),
                kind: JsonMemberKind::Enum {
                    enum_type: "public::Gender".into(),
                },
            }]),
            is_free_object: false,
        };
        let Value::Object(obj) = decode(&shape, &raw) else {
            panic!("expected Object")
        };
        assert_eq!(obj.type_name(), None);
        assert_eq!(
            obj.get("gender"),
            Some(&Value::Enum {
                type_name: "default::Gender".into(),
                value: "Female".into()
            }),
        );
    }

    #[test]
    fn decodes_group_result() {
        let value = comp(vec![
            DecodedValue::Str("Male".into()),
            DecodedValue::Array(vec![DecodedValue::Str("gender".into())]),
            DecodedValue::Array(vec![comp(vec![
                DecodedValue::Str("default::Person".into()),
                DecodedValue::Str("Bob".into()),
            ])]),
        ]);
        let shape = ShapeNode::Group {
            key_nodes: vec![ShapeNode::Scalar {
                name: "gender".into(),
                position: 0,
            }],
            grouping_position: 1,
            elements_position: 2,
            element: Box::new(ShapeNode::Object {
                name: String::new(),
                type_name: Some("default::Person".into()),
                position: 0,
                cardinality: Cardinality::Many,
                pointers: vec![
                    ShapeNode::Scalar {
                        name: "__type__".into(),
                        position: 0,
                    },
                    ShapeNode::Scalar {
                        name: "name".into(),
                        position: 1,
                    },
                ],
            }),
        };
        let Value::Group(group) = decode(&shape, &value) else {
            panic!("expected Group")
        };
        assert_eq!(group.key.get("gender"), Some(&Value::Str("Male".into())));
        assert_eq!(group.grouping, vec!["gender".to_string()]);
        assert_eq!(group.elements.len(), 1);
        let Value::Object(person) = &group.elements[0] else {
            panic!("expected Object element")
        };
        assert_eq!(person.get("name"), Some(&Value::Str("Bob".into())));
    }

    #[test]
    fn decodes_vector_search_result() {
        let obj = comp(vec![
            DecodedValue::Str("default::Product".into()),
            DecodedValue::Str("Widget".into()),
        ]);
        let value = comp(vec![DecodedValue::Null, obj, DecodedValue::F64(0.25)]);
        let shape = ShapeNode::VectorSearch {
            object_position: 1,
            distance_position: 2,
            object_node: Box::new(ShapeNode::Object {
                name: "object".into(),
                type_name: Some("default::Product".into()),
                position: 1,
                cardinality: Cardinality::Many,
                pointers: vec![
                    ShapeNode::Scalar {
                        name: "__type__".into(),
                        position: 0,
                    },
                    ShapeNode::Scalar {
                        name: "name".into(),
                        position: 1,
                    },
                ],
            }),
        };
        let Value::VectorSearch { object, distance } = decode(&shape, &value) else {
            panic!("expected VectorSearch")
        };
        assert_eq!(distance, 0.25);
        let Value::Object(product) = *object else {
            panic!("expected Object")
        };
        assert_eq!(product.get("name"), Some(&Value::Str("Widget".into())));
    }

    #[test]
    fn cached_to_value_converts_a_range() {
        let value = DecodedValue::Range {
            lower: Some(Box::new(DecodedValue::I64(1))),
            upper: Some(Box::new(DecodedValue::I64(10))),
            inc_lower: true,
            inc_upper: false,
            empty: false,
        };
        let Value::Range(range) = cached_to_value(&value) else {
            panic!("expected Range")
        };
        assert_eq!(range.lower, Some(Value::Int64(1)));
        assert_eq!(range.upper, Some(Value::Int64(10)));
        assert!(range.inc_lower && !range.inc_upper && !range.empty);
    }
}
