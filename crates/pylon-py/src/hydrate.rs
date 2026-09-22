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

//! Native hydration: decoded rows + a compiled query's shape -> the user's
//! own Python classes.
//!
//! This is the Rust counterpart of `pylon/query.py`'s `_decode`, and the two
//! must agree exactly — `tests/test_hydrate_parity.py` runs both over the
//! same rows and compares. The Python one remains the reference for what the
//! contract *is*; this one exists because the walk sits on the hot path
//! twice over: it runs on every fresh query result and on every cache hit,
//! where it is 100% of the latency.
//!
//! What makes the native version faster is not that Python object creation
//! is cheaper from Rust — it isn't, it's the same C API either way — but
//! that the interpretive overhead around it disappears. Per shape node the
//! Python version does a dict lookup for `node["kind"]`, string-compares it
//! through a chain of branches, does another dict lookup per field it reads,
//! and pays a Python call frame; several node kinds also copy the whole node
//! dict just to override `position` (`{**element, "position": 0}`). Here the
//! shape is already a typed tree, so all of that is a match on a tag.

use std::collections::HashMap;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple, PyType};

use pylon_core::query::{JsonMember, JsonMemberKind, ShapeNode};
use pylon_value::DecodedValue;

use crate::CompiledQuery;
use crate::pgvalue::cached_to_py;
use crate::rowset::RowSet;

/// The value at `index` within a composite/array row, or `None` if this
/// value isn't indexable or the index is past the end.
///
/// `_decode` expresses this as `value[position]` against a Python tuple; the
/// same access against the undecoded representation is a slice index.
fn item(value: &DecodedValue, index: usize) -> Option<&DecodedValue> {
    match value {
        DecodedValue::Composite(items) | DecodedValue::Array(items) => items.get(index),
        _ => None,
    }
}

/// The value under `key` in a jsonb object.
fn field<'a>(value: &'a DecodedValue, key: &str) -> Option<&'a DecodedValue> {
    match value {
        DecodedValue::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}

/// Whether this value is a decoded jsonb container — the condition
/// `_decode` writes as `isinstance(value, (dict, list))` to tell a
/// root-level named tuple (already the jsonb value) from a nested one
/// (sitting at a position inside the parent composite).
fn is_json_container(value: &DecodedValue) -> bool {
    matches!(value, DecodedValue::Object(_) | DecodedValue::Array(_))
}

const NULL: DecodedValue = DecodedValue::Null;

/// Per-class hydration plan, resolved once when the registry is built rather
/// than per decoded object.
///
/// `multilinks` is the reason this exists: `_install_link_sets` walks
/// `cls.__pylon_config__.pointers` for *every* object it builds, checking
/// `meta.kind != "multilink"` on each — so a 50-row result over a type with
/// 18 pointers ran 900 attribute comparisons to find the one multilink.
/// The set of multilinks on a class can't change between queries, so it is
/// resolved here instead.
struct ClassInfo {
    cls: Py<PyType>,
    /// `(pointer name, pointer meta object)` for each multilink.
    multilinks: Vec<(Py<PyString>, Py<PyAny>)>,
}

/// The registry of user classes hydration decodes into, plus the Python
/// helpers it constructs.
///
/// Built once per schema and reused across queries. `pylon.client._hydrate`
/// used to rebuild an equivalent dict on every single call — cheap in
/// isolation (0.6 µs) but pure repetition, and building it here lets the
/// per-class multilink resolution above be amortized too.
#[pyclass(module = "pylon._core", frozen)]
pub struct HydrationRegistry {
    /// Keyed by both the short name (`"Person"`) and, where the caller
    /// supplied it, the qualified one (`"default::Person"`) — matching the
    /// two lookups `_decode` does.
    classes: HashMap<String, ClassInfo>,
    /// Enum classes, looked up by qualified name first then short name.
    enums: HashMap<String, Py<PyType>>,
    link_set: Py<PyAny>,
    pylon_set: Py<PyAny>,
    named_tuple_value: Py<PyAny>,
    /// `object.__new__`, called as `object.__new__(cls)` to build an instance
    /// without running `__init__` — matching `_decode` exactly. Not
    /// `cls.__new__(cls)`: a `@pylon.type` class is a dataclass whose
    /// `__new__` is `object.__new__`, but going through the class would pick
    /// up any override a user's base class introduced.
    object_new: Py<PyAny>,
    /// `pylon.datatypes.Object`, the dataclass a free object decodes into.
    free_object: Py<PyAny>,
}

#[pymethods]
impl HydrationRegistry {
    /// `classes` maps a name to a `@pylon.type` class; `enums` maps a name to
    /// an enum class. Both accept short and qualified names, exactly as the
    /// dict `_hydrate` used to assemble did.
    #[new]
    fn new(classes: &Bound<'_, PyDict>, enums: &Bound<'_, PyDict>) -> PyResult<Self> {
        let py = classes.py();
        let datatypes = py.import("pylon.datatypes")?;

        let mut resolved = HashMap::with_capacity(classes.len());
        for (key, value) in classes.iter() {
            let name: String = key.extract()?;
            let cls = value.cast_into::<PyType>()?;
            let mut multilinks = Vec::new();
            // A class with no `__pylon_config__` is legal — `_install_link_sets`
            // returns early for one — so an absent attribute means "no
            // multilinks", not an error.
            if let Ok(cfg) = cls.getattr("__pylon_config__")
                && let Ok(pointers) = cfg.getattr("pointers")
                && let Ok(items) = pointers.call_method0("items")
            {
                for item in items.try_iter()? {
                    let item = item?;
                    let pair = item.cast_into::<PyTuple>()?;
                    let pname = pair.get_item(0)?;
                    let meta = pair.get_item(1)?;
                    if meta.getattr("kind")?.extract::<String>()? == "multilink" {
                        multilinks.push((pname.cast_into::<PyString>()?.unbind(), meta.unbind()));
                    }
                }
            }
            resolved.insert(
                name,
                ClassInfo {
                    cls: cls.unbind(),
                    multilinks,
                },
            );
        }

        let mut resolved_enums = HashMap::with_capacity(enums.len());
        for (key, value) in enums.iter() {
            resolved_enums.insert(key.extract::<String>()?, value.cast_into::<PyType>()?.unbind());
        }

        Ok(Self {
            classes: resolved,
            enums: resolved_enums,
            link_set: datatypes.getattr("LinkSet")?.unbind(),
            pylon_set: datatypes.getattr("PylonSet")?.unbind(),
            named_tuple_value: datatypes.getattr("NamedTupleValue")?.unbind(),
            object_new: py.import("builtins")?.getattr("object")?.getattr("__new__")?.unbind(),
            free_object: datatypes.getattr("Object")?.unbind(),
        })
    }
}

impl HydrationRegistry {
    /// Class for a `"module::Name"`, trying the qualified name then the
    /// short one — the same order `_decode` uses.
    fn lookup_class(&self, qualified: &str) -> Option<&ClassInfo> {
        let short = qualified.rsplit("::").next().unwrap_or(qualified);
        self.classes.get(short)
    }

    fn lookup_enum(&self, enum_type: &str) -> Option<&Py<PyType>> {
        self.enums.get(enum_type).or_else(|| {
            let short = enum_type.rsplit("::").next().unwrap_or(enum_type);
            self.enums.get(short)
        })
    }
}

/// Decodes one value against one shape node. Mirrors `_decode`.
///
/// Takes the undecoded `DecodedValue` rather than a Python object: rows
/// arrive from the driver and from the cache in exactly this form, and the
/// containers a Python conversion would build (a tuple per composite, a list
/// per array) are then immediately thrown away by this walk, which only
/// reads positions out of them. Only the leaves that survive into the result
/// are converted, by `cached_to_py`.
fn decode<'py>(
    py: Python<'py>,
    value: &DecodedValue,
    node: &ShapeNode,
    reg: &HydrationRegistry,
) -> PyResult<Bound<'py, PyAny>> {
    match node {
        ShapeNode::Scalar { position, .. } => cached_to_py(py, item(value, *position).unwrap_or(&NULL)),
        ShapeNode::RawScalar | ShapeNode::JsonScalar => cached_to_py(py, value),
        ShapeNode::Object {
            type_name,
            position,
            pointers,
            ..
        } => decode_object(py, value, *position, type_name.as_deref(), pointers, reg),
        ShapeNode::NamedTuple {
            position,
            type_name,
            members,
            ..
        } => {
            // A root-level named tuple *is* the decoded jsonb value; a nested
            // one sits at a position inside the parent composite.
            let raw = if matches!(value, DecodedValue::Null) || is_json_container(value) {
                value
            } else {
                item(value, *position).unwrap_or(&NULL)
            };
            decode_json_tuple(py, raw, type_name.as_deref(), members.as_deref(), reg)
        }
        ShapeNode::Enum {
            position, enum_type, ..
        } => {
            let raw = item(value, *position).unwrap_or(&NULL);
            if matches!(raw, DecodedValue::Null) {
                return Ok(py.None().into_bound(py));
            }
            let raw = cached_to_py(py, raw)?;
            match reg.lookup_enum(enum_type) {
                Some(cls) => cls.bind(py).call1((raw,)),
                None => Ok(raw),
            }
        }
        ShapeNode::Array { position, element, .. } => {
            let items = match item(value, *position) {
                Some(DecodedValue::Array(arr)) => arr
                    .iter()
                    // Array elements are anonymous records: each decodes as a
                    // root object, i.e. at position 0.
                    .map(|el| decode_at_root(py, el, element, reg))
                    .collect::<PyResult<Vec<_>>>()?,
                _ => Vec::new(),
            };
            reg.pylon_set.bind(py).call1((PyList::new(py, items)?,))
        }
        ShapeNode::Tuple { elements, names, .. } => {
            // An object element is indexed out first and read as its own
            // root: `decode_object` takes position 0 to mean "this is the
            // whole row", true of the query's root but not of a tuple's
            // first element.
            let items = elements
                .iter()
                .map(|e| match e {
                    ShapeNode::Object {
                        position,
                        type_name,
                        pointers,
                        ..
                    } => {
                        let element = item(value, *position).unwrap_or(&NULL);
                        decode_object(py, element, 0, type_name.as_deref(), pointers, reg)
                    }
                    _ => decode(py, value, e, reg),
                })
                .collect::<PyResult<Vec<_>>>()?;
            // A named tuple emitted as a composite still hydrates to the
            // value a named tuple gives, not to a plain tuple.
            let Some(names) = names else {
                return Ok(PyTuple::new(py, items)?.into_any());
            };
            let kwargs = PyDict::new(py);
            for (name, item) in names.iter().zip(items) {
                kwargs.set_item(name, item)?;
            }
            reg.named_tuple_value.bind(py).call((), Some(&kwargs))
        }
        ShapeNode::Group {
            key_nodes,
            grouping_position,
            elements_position,
            element,
        } => {
            let key_obj = PyDict::new(py);
            for kn in key_nodes {
                key_obj.set_item(shape_node_name(kn), decode(py, value, kn, reg)?)?;
            }
            let grouping = match item(value, *grouping_position) {
                Some(DecodedValue::Array(arr)) => PyList::new(
                    py,
                    arr.iter().map(|v| cached_to_py(py, v)).collect::<PyResult<Vec<_>>>()?,
                )?,
                _ => PyList::empty(py),
            };
            let elements = match item(value, *elements_position) {
                Some(DecodedValue::Array(arr)) => arr
                    .iter()
                    .map(|el| decode_at_root(py, el, element, reg))
                    .collect::<PyResult<Vec<_>>>()?,
                _ => Vec::new(),
            };
            let out = PyDict::new(py);
            out.set_item("key", key_obj)?;
            out.set_item("grouping", grouping)?;
            out.set_item("elements", PyList::new(py, elements)?)?;
            Ok(out.into_any())
        }
        ShapeNode::VectorSearch {
            object_position,
            distance_position,
            object_node,
        } => {
            let obj = decode_at_root(py, item(value, *object_position).unwrap_or(&NULL), object_node, reg)?;
            let out = PyDict::new(py);
            out.set_item("object", obj)?;
            out.set_item(
                "distance",
                cached_to_py(py, item(value, *distance_position).unwrap_or(&NULL))?,
            )?;
            Ok(out.into_any())
        }
        ShapeNode::FtsSearch {
            object_position,
            rank_position,
            object_node,
        } => {
            let obj = decode_at_root(py, item(value, *object_position).unwrap_or(&NULL), object_node, reg)?;
            let out = PyDict::new(py);
            out.set_item("object", obj)?;
            out.set_item("score", cached_to_py(py, item(value, *rank_position).unwrap_or(&NULL))?)?;
            Ok(out.into_any())
        }
    }
}

/// Decodes `node` as if it sat at position 0 — what `_decode` expresses as
/// `{**node, "position": 0}`.
fn decode_at_root<'py>(
    py: Python<'py>,
    value: &DecodedValue,
    node: &ShapeNode,
    reg: &HydrationRegistry,
) -> PyResult<Bound<'py, PyAny>> {
    match node {
        ShapeNode::Object {
            type_name, pointers, ..
        } => decode_object(py, value, 0, type_name.as_deref(), pointers, reg),
        ShapeNode::NamedTuple { type_name, members, .. } => {
            let raw = if matches!(value, DecodedValue::Null) || is_json_container(value) {
                value
            } else {
                item(value, 0).unwrap_or(&NULL)
            };
            decode_json_tuple(py, raw, type_name.as_deref(), members.as_deref(), reg)
        }
        // An enum inside an array *is* the label, not a field of a record, so
        // there is no composite to read a position out of.
        ShapeNode::Enum { enum_type, .. } if !matches!(value, DecodedValue::Composite(_)) => {
            if matches!(value, DecodedValue::Null) {
                return Ok(py.None().into_bound(py));
            }
            let raw = cached_to_py(py, value)?;
            match reg.lookup_enum(enum_type) {
                Some(cls) => cls.bind(py).call1((raw,)),
                None => Ok(raw),
            }
        }
        _ => decode(py, value, node, reg),
    }
}

fn decode_object<'py>(
    py: Python<'py>,
    value: &DecodedValue,
    position: usize,
    type_name: Option<&str>,
    pointers: &[ShapeNode],
    reg: &HydrationRegistry,
) -> PyResult<Bound<'py, PyAny>> {
    // The root object is the whole tuple; a nested one sits at a position.
    let obj_tuple = if position == 0 {
        value
    } else {
        item(value, position).unwrap_or(&NULL)
    };
    if matches!(obj_tuple, DecodedValue::Null) {
        return Ok(py.None().into_bound(py));
    }

    let kwargs = PyDict::new(py);
    for p in pointers {
        // pointers[0] is the auto-injected __type__ discriminator at
        // position 0 and is skipped; an explicitly-requested __type__ sits
        // at a position > 0 and is kept.
        if shape_node_name(p) == "__type__" && shape_node_position(p) == Some(0) {
            continue;
        }
        kwargs.set_item(shape_node_name(p), decode(py, obj_tuple, p, reg)?)?;
    }

    // A free object literal (`select { a := 1 }`) has no schema type, so no
    // injected discriminator either — position 0 there is the first user
    // field, not a type name. Only schema-backed objects carry one. What
    // comes back is a `pylon.Object` — a dataclass built from the field
    // names — so a free object reads through attributes the way a
    // schema-backed row does.
    let Some(static_type_name) = type_name else {
        return reg.free_object.bind(py).call((), Some(&kwargs));
    };

    // The per-row `__type__` is what a polymorphic query needs: for a
    // concrete type it equals the static name, for an interface query it
    // names the real concrete type.
    let resolved: &str = match item(obj_tuple, 0) {
        Some(DecodedValue::Str(s)) => s,
        _ => static_type_name,
    };
    let Some(info) = reg.lookup_class(resolved) else {
        return Ok(kwargs.into_any());
    };

    let obj = reg.object_new.bind(py).call1((info.cls.bind(py),))?;
    let dict = obj.getattr("__dict__")?.cast_into::<PyDict>()?;
    dict.update(kwargs.as_mapping())?;
    dict.set_item("__pylon_type__", resolved)?;
    // Shadow copy of the hydrated values, so `Client.save()` can diff
    // current against persisted state instead of intercepting __setattr__.
    let saved = kwargs.copy()?;
    dict.set_item("__pylon_saved__", &saved)?;

    // Every multilink gets a LinkSet: a hydrated one when the shape asked
    // for it, an unhydrated placeholder otherwise, so reading one that
    // wasn't fetched says so instead of raising AttributeError. Multilinks
    // are excluded from __pylon_saved__ — the LinkSet's own op log tracks
    // them, not diffing.
    for (name, meta) in &info.multilinks {
        let name = name.bind(py);
        let link_set = match kwargs.get_item(name)? {
            Some(members) => {
                let members = if members.is_none() {
                    PyTuple::empty(py).into_any()
                } else {
                    members
                };
                let kw = PyDict::new(py);
                kw.set_item("pointer", meta)?;
                reg.link_set.bind(py).call((members,), Some(&kw))?
            }
            None => {
                let kw = PyDict::new(py);
                kw.set_item("unhydrated", true)?;
                kw.set_item("pointer", meta)?;
                reg.link_set.bind(py).call((), Some(&kw))?
            }
        };
        dict.set_item(name, link_set)?;
        saved.del_item(name).ok();
    }
    Ok(obj)
}

/// Mirrors `_decode_json_tuple`.
fn decode_json_tuple<'py>(
    py: Python<'py>,
    value: &DecodedValue,
    type_name: Option<&str>,
    members: Option<&[JsonMember]>,
    reg: &HydrationRegistry,
) -> PyResult<Bound<'py, PyAny>> {
    if matches!(value, DecodedValue::Null) {
        return Ok(py.None().into_bound(py));
    }
    let Some(members) = members else {
        // No static member shape: hand back the raw jsonb value, except that
        // a registered nominal type still gets constructed from it.
        if let Some(name) = type_name
            && matches!(value, DecodedValue::Object(_))
            && let Some(info) = reg.classes.get(name)
        {
            let kwargs = cached_to_py(py, value)?;
            return info.cls.bind(py).call((), Some(kwargs.cast::<PyDict>()?));
        }
        return cached_to_py(py, value);
    };

    let positional = members.iter().all(|m| m.key.is_none());
    if positional {
        let items = members
            .iter()
            .enumerate()
            .map(|(i, m)| decode_json_member(py, item(value, i).unwrap_or(&NULL), m, reg))
            .collect::<PyResult<Vec<_>>>()?;
        return Ok(PyTuple::new(py, items)?.into_any());
    }

    let kwargs = PyDict::new(py);
    for m in members {
        let key = m.key.as_deref().unwrap_or_default();
        let raw = field(value, key).unwrap_or(&NULL);
        kwargs.set_item(key, decode_json_member(py, raw, m, reg)?)?;
    }
    if let Some(name) = type_name
        && let Some(info) = reg.classes.get(name)
    {
        return info.cls.bind(py).call((), Some(&kwargs));
    }
    reg.named_tuple_value.bind(py).call((), Some(&kwargs))
}

/// Mirrors `_decode_json_member`.
fn decode_json_member<'py>(
    py: Python<'py>,
    value: &DecodedValue,
    member: &JsonMember,
    reg: &HydrationRegistry,
) -> PyResult<Bound<'py, PyAny>> {
    match &member.kind {
        // "scalar" — jsonb's own native JSON type is already correct.
        JsonMemberKind::Scalar => cached_to_py(py, value),
        JsonMemberKind::Enum { enum_type } => {
            if matches!(value, DecodedValue::Null) {
                return Ok(py.None().into_bound(py));
            }
            let raw = cached_to_py(py, value)?;
            match reg.lookup_enum(enum_type) {
                Some(cls) => cls.bind(py).call1((raw,)),
                None => Ok(raw),
            }
        }
        JsonMemberKind::Tuple { type_name, members } => {
            decode_json_tuple(py, value, type_name.as_deref(), Some(members), reg)
        }
    }
}

fn shape_node_name(node: &ShapeNode) -> &str {
    match node {
        ShapeNode::Scalar { name, .. }
        | ShapeNode::Object { name, .. }
        | ShapeNode::Array { name, .. }
        | ShapeNode::NamedTuple { name, .. }
        | ShapeNode::Enum { name, .. } => name,
        _ => "",
    }
}

fn shape_node_position(node: &ShapeNode) -> Option<usize> {
    match node {
        ShapeNode::Scalar { position, .. }
        | ShapeNode::Object { position, .. }
        | ShapeNode::Array { position, .. }
        | ShapeNode::NamedTuple { position, .. }
        | ShapeNode::Enum { position, .. }
        | ShapeNode::Tuple { position, .. } => Some(*position),
        _ => None,
    }
}

/// Decodes every row against `compiled`'s shape — the native equivalent of
/// `pylon.query.deserialize`.
#[pyfunction]
pub(crate) fn hydrate<'py>(
    py: Python<'py>,
    rows: &RowSet,
    compiled: &CompiledQuery,
    registry: &HydrationRegistry,
) -> PyResult<Bound<'py, PyList>> {
    let root = &compiled.inner.shape.root;
    let decoded = rows
        .rows
        .iter()
        .map(|row| decode(py, row, root, registry))
        .collect::<PyResult<Vec<_>>>()?;
    PyList::new(py, decoded)
}

/// Every row as a JSON document, rendered from the same shape `hydrate`
/// reads — so the query runs once, and objects keep their pointers' names.
#[pyfunction]
pub(crate) fn rows_to_json(rows: &RowSet, compiled: &CompiledQuery) -> Vec<String> {
    let root = &compiled.inner.shape.root;
    rows.rows.iter().map(|row| pylon_client::json::row_to_json(root, row)).collect()
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<HydrationRegistry>()?;
    m.add_function(wrap_pyfunction!(hydrate, m)?)?;
    m.add_function(wrap_pyfunction!(rows_to_json, m)?)?;
    Ok(())
}
