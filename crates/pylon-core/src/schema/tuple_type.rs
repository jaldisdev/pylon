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

//! The PostgreSQL composite type a tuple compiles to.
//!
//! A tuple that has to persist — a property's column, an element of an
//! `array<tuple<…>>` — needs a type the database knows by name, because only
//! then does every member keep its own PostgreSQL type. Stored as `jsonb` a
//! tuple has one number type for all of its numbers, so a `decimal` member
//! loses its scale and reads back indistinguishable from a `float64`.
//!
//! A tuple that never leaves a query (a literal, a computed, a cast target)
//! needs no type of its own: an anonymous `ROW(…)` already carries one type
//! OID per field on the wire.
//!
//! **Where the types live.** In the Postgres schema of the module that
//! declares them, beside that module's enums and domains — so a module's DDL
//! is self-contained, a dropped module takes its types with it, and the
//! migration diff sees them like any other object it owns.
//!
//! **What identifies them.**
//!
//! * A *nominal* tuple — a `@pylon.named_tuple` class — is identified by its
//!   name, like every other thing a user declares: `geo::Point` becomes
//!   `"geo"."Point_t"`. The `_t` suffix is not decoration: a table's row type
//!   already occupies the bare name in its schema, so a `@pylon.type Point`
//!   and a `@pylon.named_tuple Point` in one module would collide outright.
//! * A *structural* tuple — `pylon.Tuple[...]` — has no name to be known by,
//!   so it is identified by its content: `"geo"."t_<hash of the signature>"`.
//!   Two properties of the same shape share one type; changing the shape is a
//!   new type rather than an alteration of the old one.

use std::collections::{BTreeMap, HashSet};

use sha2::{Digest, Sha256};

use crate::error::{Position, PyQLError, PyQLTypeError};
use crate::schema::{PropertyDescriptor, SchemaDescriptor, TupleMemberDescriptor, TupleMemberKind};

/// Length of a structural type name's hex digest. Matches `query_shape_id`'s
/// own width — 64 bits, far more than enough to keep a realistic number of
/// distinct tuple shapes collision-free, and short enough that the generated
/// name still reads as a name.
const HASH_HEX_LEN: usize = 16;

/// The `__nt__:module::Name` marker a nominal-tuple-typed property (or tuple
/// member) carries in place of a PostgreSQL type it has no name for yet.
const NOMINAL_MARKER: &str = "__nt__:";

/// One PostgreSQL composite type, ready for DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TupleType {
    /// The Pylon module whose Postgres schema holds this type.
    pub module: String,
    /// Unquoted type name — `Point_t` for a nominal tuple, `t_<hash>` for a
    /// structural one.
    pub name: String,
    pub attributes: Vec<TupleAttribute>,
    /// The tuple as a reader would write it (`tuple<name: text, value:
    /// numeric>`), carried into a `COMMENT ON TYPE` so a generated
    /// `t_<hash>` is still legible in psql — and, for a structural tuple,
    /// the exact string its name hashes.
    pub signature: String,
    /// `Some("geo::Point")` when this type comes from a declared named
    /// tuple; `None` for a structural one.
    pub nominal: Option<String>,
}

/// One attribute of a composite type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TupleAttribute {
    /// Unquoted attribute name. A positional tuple's members have no name of
    /// their own and a composite attribute must have one, so they are named
    /// for their index — `"0"`, `"1"`, … — which keeps the attribute order
    /// and the member order the same thing.
    pub name: String,
    /// The attribute's PostgreSQL type, quoted where it needs to be.
    pub pg_type: String,
}

/// `"geo"."Point_t"` — a composite type as an expression refers to it.
pub fn type_ref(module: &str, name: &str) -> String {
    format!("{}.{}", crate::sql::pg_schema_str(module), qi(name))
}

/// The type name a declared named tuple compiles to (unquoted).
pub fn nominal_name(name: &str) -> String {
    format!("{}_t", name)
}

/// The type name a structural tuple compiles to (unquoted) — its content,
/// hashed. The declaring module is deliberately *not* part of the hash: the
/// same shape declared in two modules then gets the same name in each of
/// their schemas, which makes the duplication legible rather than hiding it
/// behind two unrelated digests.
pub fn structural_name(members: &[TupleMemberDescriptor]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(signature(members).as_bytes());
    let digest = hasher.finalize();
    format!("t_{}", hex::encode(&digest[..HASH_HEX_LEN / 2]))
}

/// The tuple as a reader would write it, and — for a structural tuple — the
/// exact string that identifies it.
///
/// Members are rendered by the type they *mean*, not by the type they happen
/// to be spelled as: a nominal member contributes its qualified Pylon name
/// (its identity is its name), a structural one is inlined, and a plain
/// scalar contributes the PostgreSQL type it resolves to. So two spellings
/// that compile to the same column are one type, and two named tuples with
/// identical members stay two types.
pub fn signature(members: &[TupleMemberDescriptor]) -> String {
    let parts: Vec<String> = members
        .iter()
        .map(|m| match &m.name {
            Some(name) => format!("{}: {}", name, member_signature_type(&m.kind)),
            None => member_signature_type(&m.kind),
        })
        .collect();
    format!("tuple<{}>", parts.join(", "))
}

fn member_signature_type(kind: &TupleMemberKind) -> String {
    match kind {
        TupleMemberKind::Scalar { pg_type } => match split_nominal_marker(pg_type) {
            Some((qname, is_array)) => format!("{}{}", qname, if is_array { "[]" } else { "" }),
            None => pg_type.clone(),
        },
        TupleMemberKind::Enum { module, name } => format!("{}::{}", module, name),
        TupleMemberKind::NamedTuple { module, name } => format!("{}::{}", module, name),
        TupleMemberKind::Tuple { members } => signature(members),
    }
}

/// The composite type a member that is itself a tuple has — `None` for a
/// member that is a plain scalar or an enum.
///
/// A tuple literal nested inside another has to name this type: PostgreSQL
/// will not cast the inner anonymous `record` to it on the way into the
/// outer row ("cannot cast type record to …").
pub fn member_type_ref(kind: &TupleMemberKind, declaring_module: &str) -> Option<String> {
    match kind {
        TupleMemberKind::NamedTuple { module, name } => Some(type_ref(module, &nominal_name(name))),
        TupleMemberKind::Tuple { members } => Some(type_ref(declaring_module, &structural_name(members))),
        TupleMemberKind::Scalar { .. } | TupleMemberKind::Enum { .. } => None,
    }
}

/// The PostgreSQL type one member gets as a composite attribute.
///
/// `declaring_module` is the module whose schema holds the *enclosing* type,
/// which is where a nested structural tuple's own type goes too — it has no
/// declaration site of its own to be placed by.
fn member_ddl_type(kind: &TupleMemberKind, declaring_module: &str) -> String {
    match kind {
        // Already a PostgreSQL type (`numeric`, `text[]`, a quoted enum or
        // domain name) — except for a nominal tuple nested inside an array,
        // which reaches here as the marker its property would carry.
        TupleMemberKind::Scalar { pg_type } => match split_nominal_marker(pg_type) {
            Some((qname, is_array)) => {
                let (module, name) = split_qname(qname);
                format!(
                    "{}{}",
                    type_ref(module, &nominal_name(name)),
                    if is_array { "[]" } else { "" }
                )
            }
            None => pg_type.clone(),
        },
        TupleMemberKind::Enum { module, name } => type_ref(module, name),
        TupleMemberKind::NamedTuple { module, name } => type_ref(module, &nominal_name(name)),
        TupleMemberKind::Tuple { members } => type_ref(declaring_module, &structural_name(members)),
    }
}

/// The composite type a tuple-typed property's column has — `None` for every
/// other property.
///
/// `owner_module` is the module of the type that declares the property, which
/// is where a structural tuple's own type lives. An `array<tuple<…>>`
/// property keeps the array on the outside: one composite per element.
pub fn property_column_type(prop: &PropertyDescriptor, owner_module: &str) -> Option<String> {
    let is_array = prop.pg_type.ends_with("[]");
    let base = prop.pg_type.strip_suffix("[]").unwrap_or(&prop.pg_type);
    let type_ref = if let Some(qname) = base.strip_prefix(NOMINAL_MARKER) {
        let (module, name) = split_qname(qname);
        type_ref(module, &nominal_name(name))
    } else {
        // A structural tuple property is a plain `jsonb` column today, so
        // `tuple_members` is the only thing that marks it as a tuple at all.
        let members = prop.tuple_members.as_ref()?;
        type_ref(owner_module, &structural_name(members))
    };
    Some(if is_array { format!("{}[]", type_ref) } else { type_ref })
}

/// Every composite type the schema needs, each one after the types it refers
/// to, so the list can be emitted as DDL in order.
///
/// Both declared named tuples and the structural tuples properties carry are
/// collected: a declared one gets its type whether or not a column uses it,
/// the same way a declared enum does, so a cast to it (`<geo::Point>$p`) has
/// something to name.
pub fn collect(schema: &SchemaDescriptor) -> Result<Vec<TupleType>, PyQLError> {
    let mut out: Vec<TupleType> = Vec::new();
    // Keyed by (module, name) — the identity of the Postgres type itself, so
    // two properties of one shape in one module collect once.
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for nt in &schema.named_tuples {
        collect_nominal(&nt.module, &nt.name, schema, &mut out, &mut seen, &mut Vec::new())?;
    }
    for t in &schema.types {
        for p in &t.properties {
            let base = p.pg_type.strip_suffix("[]").unwrap_or(&p.pg_type);
            if base.starts_with(NOMINAL_MARKER) {
                // Collected above, from the declaration itself.
                continue;
            }
            if let Some(members) = &p.tuple_members {
                collect_structural(members, &t.module, schema, &mut out, &mut seen, &mut Vec::new())?;
            }
        }
    }
    Ok(out)
}

/// Depth-first, so a type is pushed only once everything it refers to has
/// been. `path` carries the nominal tuples currently being visited, which is
/// what makes a cycle visible: PostgreSQL has no composite type that contains
/// itself, and a dataclass with a forward reference can ask for one.
fn collect_nominal(
    module: &str,
    name: &str,
    schema: &SchemaDescriptor,
    out: &mut Vec<TupleType>,
    seen: &mut HashSet<(String, String)>,
    path: &mut Vec<String>,
) -> Result<(), PyQLError> {
    let qname = format!("{}::{}", module, name);
    if let Some(at) = path.iter().position(|p| *p == qname) {
        let cycle = path[at..].join(" → ");
        return Err(PyQLError::Type(PyQLTypeError {
            message: format!("named tuple '{}' contains itself, through {} → {}", qname, cycle, qname),
            position: Position { line: 0, col: 0 },
        }));
    }
    let key = (module.to_string(), nominal_name(name));
    if seen.contains(&key) {
        return Ok(());
    }
    let Some(nt) = schema
        .named_tuples
        .iter()
        .find(|nt| nt.module == module && nt.name == name)
    else {
        // An unregistered named tuple is a declaration error of its own,
        // reported where the reference is resolved; there is no type to
        // collect for it here.
        return Ok(());
    };

    path.push(qname.clone());
    collect_member_deps(&nt.members, module, schema, out, seen, path)?;
    path.pop();

    seen.insert(key);
    out.push(TupleType {
        module: module.to_string(),
        name: nominal_name(name),
        attributes: attributes(&nt.members, module),
        signature: signature(&nt.members),
        nominal: Some(qname),
    });
    Ok(())
}

fn collect_structural(
    members: &[TupleMemberDescriptor],
    module: &str,
    schema: &SchemaDescriptor,
    out: &mut Vec<TupleType>,
    seen: &mut HashSet<(String, String)>,
    path: &mut Vec<String>,
) -> Result<(), PyQLError> {
    let name = structural_name(members);
    let key = (module.to_string(), name.clone());
    if seen.contains(&key) {
        return Ok(());
    }
    collect_member_deps(members, module, schema, out, seen, path)?;
    seen.insert(key);
    out.push(TupleType {
        module: module.to_string(),
        name,
        attributes: attributes(members, module),
        signature: signature(members),
        nominal: None,
    });
    Ok(())
}

fn collect_member_deps(
    members: &[TupleMemberDescriptor],
    module: &str,
    schema: &SchemaDescriptor,
    out: &mut Vec<TupleType>,
    seen: &mut HashSet<(String, String)>,
    path: &mut Vec<String>,
) -> Result<(), PyQLError> {
    for m in members {
        match &m.kind {
            TupleMemberKind::NamedTuple { module, name } => {
                collect_nominal(module, name, schema, out, seen, path)?;
            }
            TupleMemberKind::Tuple { members } => {
                collect_structural(members, module, schema, out, seen, path)?;
            }
            // A nominal tuple reached through an array member carries the
            // same marker a property would (`__nt__:geo::Point[]`).
            TupleMemberKind::Scalar { pg_type } => {
                if let Some((qname, _)) = split_nominal_marker(pg_type) {
                    let (nt_module, nt_name) = split_qname(qname);
                    collect_nominal(nt_module, nt_name, schema, out, seen, path)?;
                }
            }
            TupleMemberKind::Enum { .. } => {}
        }
    }
    Ok(())
}

fn attributes(members: &[TupleMemberDescriptor], declaring_module: &str) -> Vec<TupleAttribute> {
    members
        .iter()
        .enumerate()
        .map(|(i, m)| TupleAttribute {
            name: m.name.clone().unwrap_or_else(|| i.to_string()),
            pg_type: member_ddl_type(&m.kind, declaring_module),
        })
        .collect()
}

/// `__nt__:geo::Point[]` -> `("geo::Point", true)`; anything else -> `None`.
fn split_nominal_marker(pg_type: &str) -> Option<(&str, bool)> {
    let marker = pg_type.strip_prefix(NOMINAL_MARKER)?;
    Some(match marker.strip_suffix("[]") {
        Some(base) => (base, true),
        None => (marker, false),
    })
}

/// `geo::Point` -> `("geo", "Point")`. An unqualified name belongs to the
/// `default` module, matching how every other reference resolves.
fn split_qname(qname: &str) -> (&str, &str) {
    match qname.split_once("::") {
        Some((module, name)) => (module, name),
        None => ("default", qname),
    }
}

fn qi(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Every collected type keyed by the `(schema, name)` pair a database
/// identifies it by — what the migration diff compares against what the
/// database already has.
pub fn collect_by_pg_name(schema: &SchemaDescriptor) -> Result<BTreeMap<(String, String), TupleType>, PyQLError> {
    Ok(collect(schema)?
        .into_iter()
        .map(|t| ((pg_schema_name(&t.module), t.name.clone()), t))
        .collect())
}

/// The Postgres schema a module's objects live in, unquoted — `default` is
/// `public`, every other module is its own name (see `pg_schema_mapping`).
pub fn pg_schema_name(module: &str) -> String {
    if module == "default" {
        "public".to_string()
    } else {
        module.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{NamedTupleDescriptor, TypeDescriptor};

    fn member(name: Option<&str>, kind: TupleMemberKind) -> TupleMemberDescriptor {
        TupleMemberDescriptor {
            name: name.map(str::to_string),
            kind,
        }
    }

    fn scalar(name: Option<&str>, pg_type: &str) -> TupleMemberDescriptor {
        member(
            name,
            TupleMemberKind::Scalar {
                pg_type: pg_type.to_string(),
            },
        )
    }

    fn named_tuple(module: &str, name: &str, members: Vec<TupleMemberDescriptor>) -> NamedTupleDescriptor {
        NamedTupleDescriptor {
            name: name.to_string(),
            module: module.to_string(),
            members,
        }
    }

    fn property(name: &str, pg_type: &str, tuple_members: Option<Vec<TupleMemberDescriptor>>) -> PropertyDescriptor {
        PropertyDescriptor {
            name: name.to_string(),
            pg_type: pg_type.to_string(),
            nullable: true,
            default_sql: None,
            default_pyql: None,
            description: None,
            check_constraints: vec![],
            is_exclusive: false,
            is_pk: false,
            is_readonly: false,
            rewrites: vec![],
            tuple_members,
            column_type: None,
        }
    }

    fn type_with(module: &str, name: &str, properties: Vec<PropertyDescriptor>) -> TypeDescriptor {
        TypeDescriptor {
            name: name.to_string(),
            module: module.to_string(),
            table: name.to_string(),
            abstract_: false,
            materialized: false,
            description: None,
            parents: vec![],
            interfaces: vec![],
            bases: vec![],
            properties,
            links: vec![],
            multilinks: vec![],
            computed: vec![],
            constraints: vec![],
            indexes: vec![],
            partition: None,
            vector_indexes: vec![],
            search_indexes: vec![],
            triggers: vec![],
            junction: false,
            signals: vec![],
        }
    }

    #[test]
    fn a_declared_named_tuple_is_named_for_itself_in_its_own_module() {
        let mut schema = SchemaDescriptor::default();
        schema
            .named_tuples
            .push(named_tuple("geo", "Point", vec![scalar(Some("x"), "int8")]));

        let types = collect(&schema).unwrap();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name, "Point_t");
        assert_eq!(types[0].module, "geo");
        assert_eq!(type_ref(&types[0].module, &types[0].name), "\"geo\".\"Point_t\"");
    }

    #[test]
    fn the_default_module_lands_in_the_public_schema() {
        assert_eq!(type_ref("default", "Point_t"), "\"public\".\"Point_t\"");
        assert_eq!(pg_schema_name("default"), "public");
    }

    #[test]
    fn a_structural_tuple_is_named_for_its_content() {
        let members = vec![scalar(Some("name"), "text"), scalar(Some("value"), "numeric")];

        let name = structural_name(&members);
        assert!(name.starts_with("t_"), "got {name}");
        assert_eq!(name.len(), 2 + HASH_HEX_LEN);
        // Stable: the same shape asked twice is the same type.
        assert_eq!(name, structural_name(&members));
    }

    #[test]
    fn a_different_member_type_is_a_different_structural_type() {
        let decimal = vec![scalar(Some("amount"), "numeric")];
        let float = vec![scalar(Some("amount"), "float8")];
        assert_ne!(
            structural_name(&decimal),
            structural_name(&float),
            "a decimal member and a float member are the whole point of having a type at all"
        );
    }

    #[test]
    fn naming_the_members_makes_it_a_different_type_from_the_positional_one() {
        let named = vec![scalar(Some("name"), "text"), scalar(Some("value"), "numeric")];
        let positional = vec![scalar(None, "text"), scalar(None, "numeric")];
        assert_ne!(structural_name(&named), structural_name(&positional));
    }

    #[test]
    fn a_positional_members_attribute_is_named_for_its_index() {
        let members = vec![scalar(None, "numeric"), scalar(None, "text")];

        let attrs = attributes(&members, "default");
        assert_eq!(
            attrs,
            vec![
                TupleAttribute {
                    name: "0".into(),
                    pg_type: "numeric".into()
                },
                TupleAttribute {
                    name: "1".into(),
                    pg_type: "text".into()
                },
            ]
        );
    }

    #[test]
    fn the_signature_reads_as_the_tuple_was_written() {
        assert_eq!(
            signature(&[scalar(Some("name"), "text"), scalar(Some("value"), "numeric")]),
            "tuple<name: text, value: numeric>"
        );
        assert_eq!(
            signature(&[scalar(None, "text"), scalar(None, "numeric")]),
            "tuple<text, numeric>"
        );
    }

    #[test]
    fn an_enum_member_takes_the_enums_own_type() {
        let members = vec![member(
            Some("gender"),
            TupleMemberKind::Enum {
                module: "default".into(),
                name: "Gender".into(),
            },
        )];

        assert_eq!(attributes(&members, "default")[0].pg_type, "\"public\".\"Gender\"");
        assert_eq!(signature(&members), "tuple<gender: default::Gender>");
    }

    #[test]
    fn a_nested_structural_tuple_gets_a_type_of_its_own_before_its_parent() {
        let mut schema = SchemaDescriptor::default();
        let inner = vec![scalar(Some("amount"), "numeric")];
        let outer = vec![
            member(Some("deep"), TupleMemberKind::Tuple { members: inner.clone() }),
            scalar(Some("note"), "text"),
        ];
        schema.types.push(type_with(
            "default",
            "Order",
            vec![property("t", "jsonb", Some(outer.clone()))],
        ));

        let types = collect(&schema).unwrap();
        let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
        let inner_name = structural_name(&inner);
        let outer_name = structural_name(&outer);
        assert_eq!(names, vec![inner_name.as_str(), outer_name.as_str()], "nested first");
        // The parent's attribute refers to the nested type, not to jsonb.
        assert_eq!(types[1].attributes[0].pg_type, format!("\"public\".\"{}\"", inner_name));
        assert_eq!(types[1].signature, "tuple<deep: tuple<amount: numeric>, note: text>");
    }

    #[test]
    fn a_nominal_member_refers_to_the_declared_type_rather_than_inlining_it() {
        let mut schema = SchemaDescriptor::default();
        schema
            .named_tuples
            .push(named_tuple("geo", "Point", vec![scalar(Some("x"), "int8")]));
        schema.named_tuples.push(named_tuple(
            "default",
            "Pin",
            vec![member(
                Some("at"),
                TupleMemberKind::NamedTuple {
                    module: "geo".into(),
                    name: "Point".into(),
                },
            )],
        ));

        let types = collect(&schema).unwrap();
        let pin = types.iter().find(|t| t.name == "Pin_t").unwrap();
        assert_eq!(pin.attributes[0].pg_type, "\"geo\".\"Point_t\"");
        assert_eq!(pin.signature, "tuple<at: geo::Point>");
        // Declared before the type that refers to it.
        let order: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(order, vec!["Point_t", "Pin_t"]);
    }

    #[test]
    fn two_named_tuples_with_the_same_members_stay_two_types() {
        let mut schema = SchemaDescriptor::default();
        schema
            .named_tuples
            .push(named_tuple("default", "Point", vec![scalar(Some("x"), "int8")]));
        schema
            .named_tuples
            .push(named_tuple("default", "Coord", vec![scalar(Some("x"), "int8")]));

        let types = collect(&schema).unwrap();
        let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Point_t", "Coord_t"], "a declared name is an identity");
    }

    #[test]
    fn one_shape_used_twice_in_a_module_collects_once() {
        let mut schema = SchemaDescriptor::default();
        let members = vec![scalar(Some("name"), "text")];
        schema.types.push(type_with(
            "default",
            "Webhook",
            vec![
                property("headers", "jsonb", Some(members.clone())),
                property("trailers", "jsonb", Some(members.clone())),
            ],
        ));

        assert_eq!(collect(&schema).unwrap().len(), 1);
    }

    #[test]
    fn one_shape_in_two_modules_gets_a_type_in_each() {
        let mut schema = SchemaDescriptor::default();
        let members = vec![scalar(Some("name"), "text")];
        schema.types.push(type_with(
            "default",
            "Webhook",
            vec![property("headers", "jsonb", Some(members.clone()))],
        ));
        schema.types.push(type_with(
            "billing",
            "Invoice",
            vec![property("headers", "jsonb", Some(members.clone()))],
        ));

        let types = collect(&schema).unwrap();
        assert_eq!(types.len(), 2);
        assert_eq!(types[0].name, types[1].name, "same shape, same name");
        let modules: Vec<&str> = types.iter().map(|t| t.module.as_str()).collect();
        assert_eq!(modules, vec!["default", "billing"], "one per module's own schema");
    }

    #[test]
    fn a_tuple_property_takes_the_composite_as_its_column_type() {
        let mut schema = SchemaDescriptor::default();
        schema
            .named_tuples
            .push(named_tuple("geo", "Point", vec![scalar(Some("x"), "int8")]));

        let nominal = property("at", "__nt__:geo::Point", None);
        assert_eq!(
            property_column_type(&nominal, "default").as_deref(),
            Some("\"geo\".\"Point_t\"")
        );

        let members = vec![scalar(Some("name"), "text")];
        let structural = property("headers", "jsonb", Some(members.clone()));
        assert_eq!(
            property_column_type(&structural, "default").as_deref(),
            Some(format!("\"public\".\"{}\"", structural_name(&members)).as_str())
        );
    }

    #[test]
    fn an_array_of_tuples_keeps_the_array_outside_the_composite() {
        let mut schema = SchemaDescriptor::default();
        schema
            .named_tuples
            .push(named_tuple("geo", "Point", vec![scalar(Some("x"), "int8")]));

        assert_eq!(
            property_column_type(&property("path", "__nt__:geo::Point[]", None), "default").as_deref(),
            Some("\"geo\".\"Point_t\"[]")
        );
        let members = vec![scalar(Some("name"), "text")];
        assert_eq!(
            property_column_type(&property("headers", "jsonb[]", Some(members.clone())), "default").as_deref(),
            Some(format!("\"public\".\"{}\"[]", structural_name(&members)).as_str())
        );
    }

    #[test]
    fn a_property_that_is_not_a_tuple_has_no_composite_type() {
        assert_eq!(property_column_type(&property("name", "text", None), "default"), None);
        assert_eq!(property_column_type(&property("tags", "text[]", None), "default"), None);
        // A plain json column is not a tuple either, despite the type it shares.
        assert_eq!(
            property_column_type(&property("payload", "jsonb", None), "default"),
            None
        );
    }

    #[test]
    fn a_nominal_tuple_inside_an_array_member_is_still_collected() {
        let mut schema = SchemaDescriptor::default();
        schema
            .named_tuples
            .push(named_tuple("geo", "Point", vec![scalar(Some("x"), "int8")]));
        let members = vec![scalar(Some("path"), "__nt__:geo::Point[]")];
        schema.types.push(type_with(
            "default",
            "Route",
            vec![property("legs", "jsonb", Some(members.clone()))],
        ));

        let types = collect(&schema).unwrap();
        let names: Vec<&str> = types.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Point_t", structural_name(&members).as_str()]);
        assert_eq!(types[1].attributes[0].pg_type, "\"geo\".\"Point_t\"[]");
        assert_eq!(types[1].signature, "tuple<path: geo::Point[]>");
    }

    #[test]
    fn a_named_tuple_that_contains_itself_is_refused() {
        let mut schema = SchemaDescriptor::default();
        schema.named_tuples.push(named_tuple(
            "default",
            "Node",
            vec![member(
                Some("parent"),
                TupleMemberKind::NamedTuple {
                    module: "default".into(),
                    name: "Node".into(),
                },
            )],
        ));

        let err = collect(&schema).unwrap_err();
        assert!(
            err.to_string().contains("contains itself"),
            "expected a cycle to be named as one, got: {err}"
        );
    }

    #[test]
    fn collecting_by_pg_name_keys_each_type_the_way_a_database_names_it() {
        let mut schema = SchemaDescriptor::default();
        schema
            .named_tuples
            .push(named_tuple("default", "Point", vec![scalar(Some("x"), "int8")]));

        let by_name = collect_by_pg_name(&schema).unwrap();
        assert!(by_name.contains_key(&("public".to_string(), "Point_t".to_string())));
    }
}
