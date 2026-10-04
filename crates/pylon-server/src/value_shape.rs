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

//! `/api/query`'s `shape` field: a compiled query's position-based
//! `ShapeNode` tree rendered as the value-tree-aligned "tag tree" the
//! frontend walks alongside the response body.
//!
//! The response carries decoded values, which is enough to *show* them but
//! not to show them as what they are: a uuid and a str are both JSON
//! strings, an enum member is its bare label, and a named tuple is
//! indistinguishable from an object. The tags carry that back, with no
//! positions and the same structure as the value itself, so `JsonTree` can
//! render `<uuid>`, `module::Enum.Member` and `(x := 1, y := 2)` for values
//! that have no schema pointer behind them to look up (a bare cast, a tuple
//! inside a free object).
//!
//! Mirrors `pylon/query.py`'s `shape_value_tags`, which stays the readable
//! statement of this contract.

use pylon_core::query::{JsonMember, JsonMemberKind, ShapeNode};
use serde_json::{Value as Json, json};

/// `public::Gender` -> `default::Gender`. `ShapeNode::Enum`'s own
/// `enum_type` is built from the Postgres schema name, where `default` is
/// spelled `public`; a `JsonMember`'s is already Pylon-qualified.
fn pylon_qualified(enum_type: &str) -> String {
    match enum_type.split_once("::") {
        Some(("public", name)) => format!("default::{name}"),
        _ => enum_type.to_string(),
    }
}

/// The tag tree for one shape node — `null` for anything whose own JSON
/// value already says what it is (a plain scalar, a raw/json column, a
/// group's own dict).
pub fn value_shape_tags(node: &ShapeNode) -> Json {
    match node {
        ShapeNode::Enum { enum_type, .. } => json!({"kind": "enum", "enumType": pylon_qualified(enum_type)}),
        ShapeNode::NamedTuple {
            type_name,
            members,
            is_free_object,
            ..
        } => {
            // A nested free object (`test := { foo := 'bar' }`) compiles to
            // the same node as a tuple literal (`test := (foo := 'bar')`) —
            // same jsonb either way — but reads as an expandable object
            // rather than a tuple literal.
            if *is_free_object {
                return json!({"kind": "object", "typeName": Json::Null, "pointers": {}});
            }
            json!({
                "kind": "namedTuple",
                "typeName": type_name.as_deref(),
                "members": members.as_ref().map(|members| {
                    members
                        .iter()
                        .map(|member| json!({"key": member.key.as_deref(), "shape": member_shape_tag(member)}))
                        .collect::<Vec<_>>()
                }),
            })
        }
        ShapeNode::Tuple { elements, .. } => json!({
            "kind": "namedTuple",
            "typeName": Json::Null,
            "members": elements
                .iter()
                .map(|element| json!({"key": Json::Null, "shape": value_shape_tags(element)}))
                .collect::<Vec<_>>(),
        }),
        ShapeNode::Object {
            type_name, pointers, ..
        } => {
            let mut tagged = serde_json::Map::new();
            for pointer in pointers {
                let name = pointer_name(pointer);
                // The auto-injected discriminator is not a value the caller
                // selected, and `to_json` drops it from the body too.
                if name == "__type__" {
                    continue;
                }
                tagged.insert(name.to_string(), value_shape_tags(pointer));
            }
            json!({"kind": "object", "typeName": type_name.as_deref(), "pointers": Json::Object(tagged)})
        }
        ShapeNode::Array { element, .. } => json!({"kind": "array", "element": value_shape_tags(element)}),
        _ => Json::Null,
    }
}

/// One member's own tag within a jsonb-backed tuple — the member kinds
/// carry their own type, so this does not go back through `value_shape_tags`.
fn member_shape_tag(member: &JsonMember) -> Json {
    match &member.kind {
        // Already Pylon-qualified, unlike `ShapeNode::Enum`'s.
        JsonMemberKind::Enum { enum_type } => json!({"kind": "enum", "enumType": enum_type}),
        JsonMemberKind::Tuple { type_name, members } => json!({
            "kind": "namedTuple",
            "typeName": type_name.as_deref(),
            "members": members
                .iter()
                .map(|member| json!({"key": member.key.as_deref(), "shape": member_shape_tag(member)}))
                .collect::<Vec<_>>(),
        }),
        JsonMemberKind::Scalar => Json::Null,
    }
}

/// A pointer's own name within its object. Only the node kinds that can
/// appear in an `Object`'s `pointers` carry one (mirrors `pylon-client`'s
/// own `pointer_name`); the rest can only ever be a shape's root.
fn pointer_name(node: &ShapeNode) -> &str {
    match node {
        ShapeNode::Scalar { name, .. }
        | ShapeNode::Enum { name, .. }
        | ShapeNode::NamedTuple { name, .. }
        | ShapeNode::Object { name, .. }
        | ShapeNode::Array { name, .. } => name,
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pylon_core::query::Cardinality;

    fn scalar(name: &str, position: usize) -> ShapeNode {
        ShapeNode::Scalar {
            name: name.to_string(),
            position,
        }
    }

    fn enum_node(name: &str, enum_type: &str) -> ShapeNode {
        ShapeNode::Enum {
            name: name.to_string(),
            position: 1,
            enum_type: enum_type.to_string(),
        }
    }

    fn named_tuple(name: &str, members: Option<Vec<JsonMember>>, type_name: Option<&str>) -> ShapeNode {
        ShapeNode::NamedTuple {
            name: name.to_string(),
            position: 1,
            type_name: type_name.map(str::to_string),
            members,
            is_free_object: false,
        }
    }

    fn member(key: Option<&str>, kind: JsonMemberKind) -> JsonMember {
        JsonMember {
            key: key.map(str::to_string),
            kind,
        }
    }

    fn object(type_name: Option<&str>, pointers: Vec<ShapeNode>) -> ShapeNode {
        ShapeNode::Object {
            name: String::new(),
            type_name: type_name.map(str::to_string),
            position: 0,
            cardinality: Cardinality::Many,
            pointers,
            has_implicit_id: false,
        }
    }

    #[test]
    fn a_plain_scalar_is_untagged() {
        assert_eq!(value_shape_tags(&scalar("name", 1)), Json::Null);
        assert_eq!(value_shape_tags(&ShapeNode::RawScalar), Json::Null);
    }

    #[test]
    fn an_enum_is_qualified_back_to_its_pylon_module() {
        assert_eq!(
            value_shape_tags(&enum_node("gender", "public::Gender")),
            json!({"kind": "enum", "enumType": "default::Gender"})
        );
        assert_eq!(
            value_shape_tags(&enum_node("status", "shop::Status")),
            json!({"kind": "enum", "enumType": "shop::Status"})
        );
    }

    #[test]
    fn a_named_tuple_carries_its_members() {
        let node = named_tuple(
            "address",
            Some(vec![
                member(Some("street"), JsonMemberKind::Scalar),
                member(Some("zip"), JsonMemberKind::Scalar),
            ]),
            None,
        );
        assert_eq!(
            value_shape_tags(&node),
            json!({
                "kind": "namedTuple",
                "typeName": Json::Null,
                "members": [{"key": "street", "shape": Json::Null}, {"key": "zip", "shape": Json::Null}],
            })
        );
    }

    #[test]
    fn a_named_tuple_with_no_member_plan_reports_none() {
        let node = named_tuple("location", None, Some("default::Point"));
        assert_eq!(
            value_shape_tags(&node),
            json!({"kind": "namedTuple", "typeName": "default::Point", "members": Json::Null})
        );
    }

    #[test]
    fn a_member_that_is_itself_a_tuple_recurses() {
        let node = named_tuple(
            "shape",
            Some(vec![member(
                Some("origin"),
                JsonMemberKind::Tuple {
                    type_name: None,
                    members: vec![
                        member(Some("x"), JsonMemberKind::Scalar),
                        member(
                            Some("y"),
                            JsonMemberKind::Enum {
                                enum_type: "default::Axis".to_string(),
                            },
                        ),
                    ],
                },
            )]),
            None,
        );
        let tags = value_shape_tags(&node);
        let origin = &tags["members"][0]["shape"];
        assert_eq!(origin["kind"], "namedTuple");
        assert_eq!(
            origin["members"][1]["shape"],
            json!({"kind": "enum", "enumType": "default::Axis"})
        );
    }

    #[test]
    fn a_free_object_reads_as_an_object_not_a_tuple_literal() {
        let node = ShapeNode::NamedTuple {
            name: "test".to_string(),
            position: 1,
            type_name: None,
            members: None,
            is_free_object: true,
        };
        assert_eq!(
            value_shape_tags(&node),
            json!({"kind": "object", "typeName": Json::Null, "pointers": {}})
        );
    }

    #[test]
    fn an_object_tags_its_pointers_and_skips_the_discriminator() {
        let node = object(
            Some("default::Person"),
            vec![
                scalar("__type__", 0),
                scalar("name", 1),
                enum_node("gender", "public::Gender"),
            ],
        );
        let tags = value_shape_tags(&node);
        assert_eq!(tags["kind"], "object");
        assert_eq!(tags["typeName"], "default::Person");
        assert!(tags["pointers"].get("__type__").is_none());
        assert_eq!(tags["pointers"]["name"], Json::Null);
        assert_eq!(
            tags["pointers"]["gender"],
            json!({"kind": "enum", "enumType": "default::Gender"})
        );
    }

    #[test]
    fn an_array_recurses_into_its_element() {
        let node = ShapeNode::Array {
            name: "headers".to_string(),
            position: 7,
            element: Box::new(named_tuple(
                "",
                Some(vec![
                    member(Some("name"), JsonMemberKind::Scalar),
                    member(Some("value"), JsonMemberKind::Scalar),
                ]),
                None,
            )),
        };
        let tags = value_shape_tags(&node);
        assert_eq!(tags["kind"], "array");
        assert_eq!(tags["element"]["kind"], "namedTuple");
        assert_eq!(tags["element"]["members"][0]["key"], "name");
    }

    #[test]
    fn a_positional_tuple_reads_as_a_named_tuple_with_no_keys() {
        let node = ShapeNode::Tuple {
            position: 0,
            elements: vec![scalar("", 0), enum_node("", "public::Gender")],
            names: None,
        };
        let tags = value_shape_tags(&node);
        assert_eq!(tags["kind"], "namedTuple");
        assert_eq!(tags["members"][0], json!({"key": Json::Null, "shape": Json::Null}));
        assert_eq!(tags["members"][1]["shape"]["enumType"], "default::Gender");
    }
}
