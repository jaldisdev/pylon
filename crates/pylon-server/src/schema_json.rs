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

//! Builds `/api/schema`/`/api/globals`'s JSON trees directly from
//! `SchemaDescriptor` — the Rust port of `pylon/server/asgi.py`'s
//! `_build_type_entry`/`_classify_pointer`/`_build_named_tuple_entry`/
//! `_global_type_text`.
//!
//! **Known, flagged gaps** (both narrow, both because the underlying
//! `SchemaDescriptor` doesn't carry the needed bit of information today —
//! see the "Phase 2" note in the project's `pylon-server` migration plan):
//! - A named-tuple member's `"required"` is always `true` —
//!   `TupleMemberDescriptor` carries no nullable/optional flag at all.
//! - A custom scalar used as an *array element* or *tuple/named-tuple
//!   member* type always renders as its base type (`"std::str"` etc.),
//!   not its own qualified name — only a property's own top-level
//!   `column_type` carries a registered scalar's identity;
//!   `TupleMemberKind::Scalar`/array pg_type strings don't.
//! - Python's `/api/schema` interleaves properties/links/multilinks/
//!   computed pointers in original declaration order; this renders them
//!   grouped (all properties, then links, then multilinks, then computed).

use pylon_core::ir::pg_type_to_pyql;
use pylon_core::schema::{
    ComputedDescriptor, GlobalDescriptor, LinkDescriptor, MultiLinkDescriptor, NamedTupleDescriptor,
    PropertyDescriptor, SchemaDescriptor, TupleMemberDescriptor, TupleMemberKind, TypeDescriptor,
    VectorIndexDescriptor,
};
use serde_json::{json, Value as Json};

fn pg_schema_to_module(pg_schema: &str) -> &str {
    if pg_schema == "public" {
        "default"
    } else {
        pg_schema
    }
}

/// `"\"schema\".\"Name\""` -> `"module::Name"` — reverses the
/// schema-qualified quoting `export`'s DDL emission applies for an
/// enum-typed column's `pg_type` and a registered scalar's `column_type`.
fn pg_quoted_to_qualified(quoted: &str) -> String {
    let inner = quoted.trim_start_matches('"');
    match inner.find("\".\"") {
        Some(idx) => {
            let module = &inner[..idx];
            let name = inner[idx + 3..].trim_end_matches('"');
            format!("{}::{}", pg_schema_to_module(module), name)
        }
        None => quoted.to_string(),
    }
}

/// A plain (non-custom) scalar's PyQL display name, or a registered custom
/// scalar's own qualified name when `column_type` names one.
fn scalar_type_name(pg_type: &str, column_type: Option<&str>) -> String {
    if let Some(ct) = column_type {
        return pg_quoted_to_qualified(ct.trim_end_matches("[]"));
    }
    pg_type_to_pyql(pg_type).to_string()
}

/// Classifies a bare pg_type string the same way `_classify_member_type`
/// does for a tuple/array member — `"scalar"`/`"enum"`/`"namedTuple"`, a
/// different kind vocabulary than a top-level pointer's own
/// `"property"`/`"link"`/etc. (matches Python's own two-function split).
fn classify_pg_type_as_member(pg_type: &str) -> Json {
    if let Some(nt_name) = pg_type.strip_prefix("__nt__:") {
        return json!({"kind": "namedTuple", "target": nt_name});
    }
    if pg_type.starts_with('"') {
        return json!({"kind": "enum", "target": pg_quoted_to_qualified(pg_type)});
    }
    json!({"kind": "scalar", "typeName": pg_type_to_pyql(pg_type)})
}

fn tuple_member_json(m: &TupleMemberDescriptor) -> Json {
    let mut obj = match &m.kind {
        TupleMemberKind::Scalar { pg_type } => json!({"kind": "scalar", "typeName": pg_type_to_pyql(pg_type)}),
        TupleMemberKind::Enum { module, name } => json!({"kind": "enum", "target": format!("{module}::{name}")}),
        TupleMemberKind::NamedTuple { module, name } => {
            json!({"kind": "namedTuple", "target": format!("{module}::{name}")})
        }
        TupleMemberKind::Tuple { members } => {
            json!({"kind": "namedTuple", "members": members.iter().map(tuple_member_json).collect::<Vec<_>>()})
        }
    };
    obj["name"] = m.name.clone().map(Json::from).unwrap_or(Json::Null);
    obj
}

fn named_tuple_json(nt: &NamedTupleDescriptor) -> Json {
    let members: Vec<Json> = nt
        .members
        .iter()
        .map(|m| {
            let mut obj = tuple_member_json(m);
            // See module doc: not derivable from `TupleMemberDescriptor` today.
            obj["required"] = Json::Bool(true);
            obj
        })
        .collect();
    json!({"module": nt.module, "name": nt.name, "members": members})
}

fn property_json(p: &PropertyDescriptor) -> Json {
    let mut obj = if let Some(nt_name) = p.pg_type.strip_prefix("__nt__:") {
        json!({"kind": "namedTuple", "target": nt_name})
    } else if p.pg_type.starts_with('"') {
        json!({"kind": "enum", "target": pg_quoted_to_qualified(&p.pg_type)})
    } else if let Some(members) = &p.tuple_members {
        json!({"kind": "namedTuple", "members": members.iter().map(tuple_member_json).collect::<Vec<_>>()})
    } else if let Some(elem_pg) = p.pg_type.strip_suffix("[]") {
        let mut element = classify_pg_type_as_member(elem_pg);
        element["name"] = Json::Null;
        json!({"kind": "array", "element": element})
    } else {
        json!({"kind": "property", "typeName": scalar_type_name(&p.pg_type, p.column_type.as_deref())})
    };
    // Every property-derived kind above is still `meta.kind == "property"`
    // at the raw pointer level, so all of them get editability fields —
    // matches `_pointer_editability`'s own `"property"` branch.
    obj["readonly"] = Json::Bool(p.is_readonly);
    obj["required"] = Json::Bool(!p.nullable);
    obj["hasDefault"] = Json::Bool(p.default_sql.is_some() || p.default_pyql.is_some());
    obj["name"] = Json::from(p.name.clone());
    obj
}

fn link_json(l: &LinkDescriptor) -> Json {
    let mut obj = json!({"kind": "link", "target": l.target, "name": l.name});
    obj["readonly"] = Json::Bool(l.is_readonly);
    obj["required"] = Json::Bool(!l.nullable);
    obj["hasDefault"] = Json::Bool(l.default_pyql.is_some());
    if let Some(through) = &l.through {
        obj["through"] = Json::from(through.clone());
    }
    obj
}

fn multilink_json(ml: &MultiLinkDescriptor) -> Json {
    let mut obj = json!({"kind": "multiLink", "target": ml.target, "name": ml.name});
    if let Some(through) = &ml.through {
        obj["through"] = Json::from(through.clone());
    }
    obj
}

fn computed_json(c: &ComputedDescriptor) -> Json {
    let mut obj = match &c.return_type {
        Some(rt) => json!({"kind": "computed", "typeName": pg_type_to_pyql(rt)}),
        None => json!({"kind": "computed"}),
    };
    obj["name"] = Json::from(c.name.clone());
    obj
}

fn vector_index_json(vi: &VectorIndexDescriptor) -> Json {
    json!({"indexName": vi.index_name, "model": vi.model, "pointers": vi.pointers})
}

fn type_json(t: &TypeDescriptor) -> Json {
    let mut bases: Vec<String> = t.parents.clone();
    bases.extend(t.interfaces.iter().cloned());

    let mut pointers: Vec<Json> = Vec::new();
    pointers.extend(t.properties.iter().map(property_json));
    pointers.extend(t.links.iter().map(link_json));
    pointers.extend(t.multilinks.iter().map(multilink_json));
    pointers.extend(t.computed.iter().map(computed_json));

    json!({
        "module": t.module,
        "name": t.name,
        "abstract": t.abstract_,
        "junction": t.junction,
        "bases": bases,
        "pointers": pointers,
        "vectorIndexes": t.vector_indexes.iter().map(vector_index_json).collect::<Vec<_>>(),
    })
}

/// `GET /api/schema`'s full response body.
pub fn schema_json(schema: &SchemaDescriptor) -> Json {
    let enums: Vec<Json> =
        schema.enums.iter().map(|e| json!({"module": e.module, "name": e.name, "members": e.members})).collect();
    json!({
        "types": schema.types.iter().map(type_json).collect::<Vec<_>>(),
        "enums": enums,
        "namedTuples": schema.named_tuples.iter().map(named_tuple_json).collect::<Vec<_>>(),
    })
}

fn global_json(g: &GlobalDescriptor) -> Json {
    // `scalar_type` is already a proper PyQL-style string as of this
    // session's `_pyql_type_name` fix in `_walker.py` — no further
    // rendering needed here (unlike the pre-fix version, which stored a
    // bare `__name__`/broken `repr()`).
    json!({"module": g.module, "name": g.name, "typeName": g.scalar_type, "required": g.required})
}

/// `GET /api/globals`'s full response body — only *settable* globals
/// (computed ones are derived at query time, never user-set).
pub fn globals_json(schema: &SchemaDescriptor) -> Json {
    let globals: Vec<Json> = schema.globals.iter().filter(|g| g.computed_expr.is_none()).map(global_json).collect();
    json!({"globals": globals})
}
