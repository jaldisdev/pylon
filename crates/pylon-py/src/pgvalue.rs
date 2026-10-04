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

//! Conversion between Python objects and `pylon_value::DecodedValue` — the
//! shared representation both `pylon-cache` (cache hits) and `pylon-pgcon`
//! (fresh rows off the wire) produce, which is what makes a cache hit and a
//! live query result the same thing to everything downstream.
//!
//! On the query hot path only *leaves* come through `cached_to_py`, called
//! from `crate::hydrate` as it walks the compiled query's shape; whole rows
//! are converted only for callers that genuinely want plain Python values
//! (`RowSet::to_list`, admin SQL, tests). `py_to_cached` is the reverse, for
//! bound query parameters and for values handed to the cache directly.

use std::sync::OnceLock;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::sync::OnceLockExt;
use pyo3::types::{
    PyBool, PyBytes, PyDate, PyDateAccess, PyDateTime, PyDict, PyFloat, PyInt, PyList, PyString, PyTime, PyTimeAccess,
    PyTuple, PyType, PyTzInfo, PyTzInfoAccess,
};

use pylon_core::query::{JsonMember, JsonMemberKind, ParamTupleType};
use pylon_value::DecodedValue;

/// Days between the Unix epoch (1970-01-01) and the PostgreSQL/Pylon epoch
/// (2000-01-01), which is what every temporal `DecodedValue` counts from.
const PG_EPOCH_DAYS_FROM_UNIX: i64 = 10_957;

const US_PER_DAY: i64 = 86_400_000_000;

/// The Python types this module constructs and type-checks against, looked
/// up once per process instead of once per value.
///
/// Every conversion used to re-run `py.import("datetime")?.getattr(...)` —
/// a `sys.modules` lookup plus an attribute lookup — for *each* value in a
/// result set, so a row with three temporal columns paid it three times. A
/// `OnceLock` behind pyo3's `get_or_init_py_attached` (rather than a plain
/// `OnceLock::get_or_init`) is the deadlock-safe form: initializing calls
/// arbitrary Python code, which must not block while another thread holds
/// the same lock.
///
/// `datetime`/`date`/`time` are deliberately absent: those are recognised
/// with pyo3's `cast::<PyDateTime>()` (a C-level `PyDateTime_Check`), which
/// needs no type object held here.
struct PyTypes {
    uuid: Py<PyType>,
    decimal: Py<PyType>,
    timedelta: Py<PyType>,
    range: Py<PyType>,
    relative_duration: Py<PyType>,
}

static PY_TYPES: OnceLock<Option<PyTypes>> = OnceLock::new();

fn py_types(py: Python<'_>) -> PyResult<&'static PyTypes> {
    // `get_or_init_py_attached` takes an infallible closure, so a failed
    // import is carried out as `None` and turned back into a `PyErr` here
    // rather than panicking while holding the lock. In practice this only
    // fails on a broken interpreter or a half-installed `pylon` package.
    PY_TYPES
        .get_or_init_py_attached(py, || {
            let load = |module: &str, name: &str| -> PyResult<Py<PyType>> {
                Ok(py.import(module)?.getattr(name)?.cast_into::<PyType>()?.unbind())
            };
            Some(PyTypes {
                uuid: load("uuid", "UUID").ok()?,
                decimal: load("decimal", "Decimal").ok()?,
                timedelta: load("datetime", "timedelta").ok()?,
                range: load("pylon.datatypes", "Range").ok()?,
                relative_duration: load("pylon.datatypes", "RelativeDuration").ok()?,
            })
        })
        .as_ref()
        .ok_or_else(|| PyValueError::new_err("could not import the stdlib types pylon decodes into"))
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
///
/// Postgres and Python's `datetime` both use the proleptic Gregorian
/// calendar, so this is exact for every date either can represent. Doing
/// the arithmetic here rather than handing `date(2000,1,1) + timedelta(n)`
/// to Python turns three Python-level calls per value into none.
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146_097 + doe - 719_468
}

/// Inverse of `days_from_civil` — `(year, month, day)` from a day count
/// since 1970-01-01.
fn civil_from_days(z: i64) -> (i32, u8, u8) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

/// Splits microseconds-since-the-Pylon-epoch into a Unix day count and the
/// microsecond offset within that day. Euclidean division so pre-2000
/// timestamps (negative input) land on the right day rather than truncating
/// toward zero.
fn split_epoch_micros(us: i64) -> (i64, u32, u8, u8, u8) {
    let days = us.div_euclid(US_PER_DAY) + PG_EPOCH_DAYS_FROM_UNIX;
    let within = us.rem_euclid(US_PER_DAY);
    let microsecond = (within % 1_000_000) as u32;
    let total_s = within / 1_000_000;
    (
        days,
        microsecond,
        (total_s / 3600) as u8,
        ((total_s / 60) % 60) as u8,
        (total_s % 60) as u8,
    )
}

/// Encodes an already-decoded Python value (from
/// any caller handing us a plain Python value to bind/cache) into
/// `DecodedValue`. Runtime-type-driven, not shape-driven — the shape is
/// only consulted later, by the unmodified `_decode()`.
pub(crate) fn py_to_cached(value: &Bound<'_, PyAny>) -> PyResult<DecodedValue> {
    let py = value.py();

    if value.is_none() {
        return Ok(DecodedValue::Null);
    }
    // Order matters: `bool` is a subclass of `int` in Python.
    if let Ok(b) = value.cast::<PyBool>() {
        return Ok(DecodedValue::Bool(b.is_true()));
    }
    if let Ok(i) = value.cast::<PyInt>() {
        return Ok(DecodedValue::I64(i.extract()?));
    }
    if let Ok(f) = value.cast::<PyFloat>() {
        return Ok(DecodedValue::F64(f.extract()?));
    }
    if let Ok(s) = value.cast::<PyString>() {
        return Ok(DecodedValue::Str(s.extract()?));
    }
    if let Ok(b) = value.cast::<PyBytes>() {
        return Ok(DecodedValue::Bytes(b.as_bytes().to_vec()));
    }
    let types = py_types(py)?;
    if value.is_instance(types.uuid.bind(py))? {
        let raw: Vec<u8> = value.getattr("bytes")?.extract()?;
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&raw);
        return Ok(DecodedValue::Uuid(bytes));
    }
    if value.is_instance(types.decimal.bind(py))? {
        return Ok(DecodedValue::Decimal(value.str()?.extract()?));
    }
    if value.is_instance(types.timedelta.bind(py))? {
        // `timedelta` only ever carries days/seconds/microseconds (Python
        // normalizes seconds into days+microseconds internally too) — never
        // months, so this always round-trips through `Interval` with
        // months == 0, matching `std::duration`'s own convention.
        let days: i32 = value.getattr("days")?.extract()?;
        let seconds: i64 = value.getattr("seconds")?.extract()?;
        let microseconds: i64 = value.getattr("microseconds")?.extract()?;
        return Ok(DecodedValue::Interval {
            months: 0,
            days,
            microseconds: seconds * 1_000_000 + microseconds,
        });
    }
    if value.is_instance(types.relative_duration.bind(py))? {
        return Ok(DecodedValue::Interval {
            months: value.getattr("months")?.extract()?,
            days: value.getattr("days")?.extract()?,
            microseconds: value.getattr("microseconds")?.extract()?,
        });
    }
    // `datetime.datetime` is a subclass of `datetime.date` — must be checked
    // first, or every datetime would also match the plain-date branch below.
    // Calendar components are read straight off the C struct via
    // `PyDateAccess`/`PyTimeAccess` and converted here (see
    // `days_from_civil`), instead of asking Python to subtract two datetimes
    // and unpack the resulting timedelta.
    if let Ok(dt) = value.cast::<PyDateTime>() {
        // A `timestamptz` is always UTC microseconds on the wire regardless
        // of the value's own tzinfo, so an aware datetime is normalized
        // first. That single `astimezone` is the only Python-level call left
        // on this path, and a naive datetime doesn't even pay it.
        let aware = dt.get_tzinfo().is_some();
        let normalized;
        let dt = if aware {
            normalized = value.call_method1("astimezone", (PyTzInfo::utc(py)?,))?;
            normalized.cast::<PyDateTime>()?
        } else {
            dt
        };
        let days = days_from_civil(dt.get_year(), dt.get_month().into(), dt.get_day().into()) - PG_EPOCH_DAYS_FROM_UNIX;
        let total_us = days * US_PER_DAY
            + (i64::from(dt.get_hour()) * 3600 + i64::from(dt.get_minute()) * 60 + i64::from(dt.get_second()))
                * 1_000_000
            + i64::from(dt.get_microsecond());
        return Ok(if aware {
            DecodedValue::Timestamptz(total_us)
        } else {
            DecodedValue::Timestamp(total_us)
        });
    }
    if let Ok(d) = value.cast::<PyDate>() {
        let days = days_from_civil(d.get_year(), d.get_month().into(), d.get_day().into()) - PG_EPOCH_DAYS_FROM_UNIX;
        return Ok(DecodedValue::Date(days as i32));
    }
    if let Ok(t) = value.cast::<PyTime>() {
        if t.get_tzinfo().is_some() {
            return Err(PyValueError::new_err(
                "cannot bind a timezone-aware datetime.time — PostgreSQL `time` (cal::local_time) has no timezone",
            ));
        }
        let total_us = (i64::from(t.get_hour()) * 3600 + i64::from(t.get_minute()) * 60 + i64::from(t.get_second()))
            * 1_000_000
            + i64::from(t.get_microsecond());
        return Ok(DecodedValue::Time(total_us));
    }
    if value.is_instance(types.range.bind(py))? {
        let empty: bool = value.getattr("empty")?.extract()?;
        let lower = value.getattr("lower")?;
        let upper = value.getattr("upper")?;
        return Ok(DecodedValue::Range {
            lower: if lower.is_none() {
                None
            } else {
                Some(Box::new(py_to_cached(&lower)?))
            },
            upper: if upper.is_none() {
                None
            } else {
                Some(Box::new(py_to_cached(&upper)?))
            },
            inc_lower: value.getattr("inc_lower")?.extract()?,
            inc_upper: value.getattr("inc_upper")?.extract()?,
            empty,
        });
    }
    if let Ok(d) = value.cast::<PyDict>() {
        let entries = d
            .iter()
            .map(|(k, v)| Ok((k.extract::<String>()?, py_to_cached(&v)?)))
            .collect::<PyResult<Vec<_>>>()?;
        return Ok(DecodedValue::Object(entries));
    }
    // A genuine Postgres array (`Array`) vs. a positional composite/record
    // (`Composite`, a plain `tuple`) matter on the
    // way back out: `_decode()`'s `"named_tuple"` case uses `isinstance(_,
    // (dict, list))` to tell "this position already holds the raw jsonb
    // value" apart from "this position holds a composite that needs
    // `value[pos]` indexing first" — a record row (and, correspondingly,
    // `DecodedValue::Composite` → `tuple`) reads as neither dict nor list,
    // which the check relies on. A `list` genuinely means "Postgres array."
    if let Ok(l) = value.cast::<PyList>() {
        let items = l.iter().map(|item| py_to_cached(&item)).collect::<PyResult<Vec<_>>>()?;
        return Ok(DecodedValue::Array(items));
    }
    if let Ok(len) = value.len() {
        let items = (0..len)
            .map(|i| py_to_cached(&value.get_item(i)?))
            .collect::<PyResult<Vec<_>>>()?;
        return Ok(DecodedValue::Composite(items));
    }
    Err(PyValueError::new_err(format!(
        "cannot convert a value of type {} to DecodedValue",
        value.get_type().name()?
    )))
}

/// Converts a query's bound parameters, reading each one that is cast to a
/// tuple type against that type's own member names.
///
/// A tuple value travels as jsonb, where a named member is a *key* — but a
/// caller holds one positionally as naturally as by name: a plain
/// `("X-Foo", "bar")`, a `@pylon.named_tuple` instance, a `NamedTupleValue`
/// read back from an earlier query, a dict. Only the cast knows the names
/// (see `ParamTupleType`), so matching the value to them happens here and
/// not in `py_to_cached`, which sees one value at a time with no idea what
/// it is being bound to.
pub(crate) fn py_params_to_cached(
    params: &[Bound<'_, PyAny>],
    param_names: &[String],
    param_tuple_types: &[Option<ParamTupleType>],
) -> PyResult<Vec<DecodedValue>> {
    params
        .iter()
        .enumerate()
        .map(
            |(i, value)| match param_tuple_types.get(i).and_then(|plan| plan.as_ref()) {
                None => py_to_cached(value),
                Some(plan) => {
                    let argument = Argument {
                        name: param_names.get(i).map(String::as_str).unwrap_or_default(),
                        value,
                    };
                    if plan.is_array {
                        tuple_array_to_cached(value, plan, &argument)
                    } else {
                        tuple_to_cached(value, &plan.members, plan.type_name.as_deref(), &argument)
                    }
                }
            },
        )
        .collect()
}

/// The argument a conversion is working inside, for an error that names it
/// the way the Python-side argument checks do (see `_check_array_elements`
/// in `client.py`).
struct Argument<'a, 'py> {
    name: &'a str,
    value: &'a Bound<'py, PyAny>,
}

impl Argument<'_, '_> {
    fn refuse(&self, detail: String) -> PyErr {
        let rendered = self
            .value
            .repr()
            .and_then(|r| r.extract::<String>())
            .unwrap_or_else(|_| "<unrepresentable>".to_string());
        invalid_parameter_type(format!(
            "invalid input for query argument ${}: {rendered} ({detail})",
            self.name
        ))
    }
}

/// `pylon.exceptions.InvalidParameterTypeError` — the error a value the
/// query cannot mean already raises on the Python side, rather than the
/// bare `ValueError` a conversion failure would otherwise surface as.
fn invalid_parameter_type(message: String) -> PyErr {
    Python::attach(|py| {
        let cls = py
            .import("pylon.exceptions")
            .and_then(|m| m.getattr("InvalidParameterTypeError"))
            .expect("pylon.exceptions must define the core exception hierarchy");
        match cls.call1((message,)) {
            Ok(instance) => PyErr::from_value(instance),
            Err(construct_err) => construct_err,
        }
    })
}

/// `<array<tuple<…>>>$p` — one tuple per element, so the member names apply
/// element-wise.
fn tuple_array_to_cached(
    value: &Bound<'_, PyAny>,
    plan: &ParamTupleType,
    argument: &Argument<'_, '_>,
) -> PyResult<DecodedValue> {
    if value.is_none() {
        return Ok(DecodedValue::Null);
    }
    let Ok(items) = value.try_iter() else {
        return Err(argument.refuse(format!(
            "a sequence of {} expected (got type '{}')",
            describe_tuple_type(&plan.members, plan.type_name.as_deref()),
            value.get_type().name()?,
        )));
    };
    let elements = items
        .map(|item| tuple_to_cached(&item?, &plan.members, plan.type_name.as_deref(), argument))
        .collect::<PyResult<Vec<_>>>()?;
    Ok(DecodedValue::Array(elements))
}

/// One tuple value as the jsonb it travels as: an object keyed by the member
/// names, or — for an all-unnamed `tuple<str, bool>` — a positional
/// `Composite`, which `wire.rs` writes as a jsonb array.
fn tuple_to_cached(
    value: &Bound<'_, PyAny>,
    members: &[JsonMember],
    type_name: Option<&str>,
    argument: &Argument<'_, '_>,
) -> PyResult<DecodedValue> {
    if value.is_none() {
        return Ok(DecodedValue::Null);
    }
    let keys: Vec<&str> = members.iter().filter_map(|m| m.key.as_deref()).collect();
    if keys.is_empty() {
        return Ok(DecodedValue::Composite(positional_members(
            value, members, type_name, argument,
        )?));
    }

    // A mapping reads by key; so does anything carrying an attribute per
    // member — an instance of a `@pylon.named_tuple` class, or a
    // `NamedTupleValue` from an earlier result. A plain tuple has neither
    // and is read by position.
    if let Ok(dict) = value.cast::<PyDict>() {
        return Ok(DecodedValue::Object(
            members
                .iter()
                .map(|member| {
                    let key = member.key.clone().unwrap_or_default();
                    let item = match dict.get_item(&key)? {
                        Some(item) => member_to_cached(&item, member, argument)?,
                        None => DecodedValue::Null,
                    };
                    Ok((key, item))
                })
                .collect::<PyResult<Vec<_>>>()?,
        ));
    }
    if keys.iter().all(|key| value.hasattr(*key).unwrap_or(false)) {
        return Ok(DecodedValue::Object(
            members
                .iter()
                .map(|member| {
                    let key = member.key.clone().unwrap_or_default();
                    let item = member_to_cached(&value.getattr(&key)?, member, argument)?;
                    Ok((key, item))
                })
                .collect::<PyResult<Vec<_>>>()?,
        ));
    }
    Ok(DecodedValue::Object(
        members
            .iter()
            .zip(positional_members(value, members, type_name, argument)?)
            .map(|(member, item)| (member.key.clone().unwrap_or_default(), item))
            .collect(),
    ))
}

/// The members read off a sequence by position — the form a plain Python
/// tuple arrives in. Arity is checked here rather than left to the database,
/// which sees only the jsonb that came out of it.
fn positional_members(
    value: &Bound<'_, PyAny>,
    members: &[JsonMember],
    type_name: Option<&str>,
    argument: &Argument<'_, '_>,
) -> PyResult<Vec<DecodedValue>> {
    let described = describe_tuple_type(members, type_name);
    let items: Vec<Bound<'_, PyAny>> = match value.try_iter() {
        Ok(iter) => iter.collect::<PyResult<Vec<_>>>()?,
        Err(_) => {
            return Err(argument.refuse(format!(
                "a mapping, a sized iterable container, or an object carrying {described}'s members \
                 expected (got type '{}')",
                value.get_type().name()?,
            )));
        }
    };
    if items.len() != members.len() {
        return Err(argument.refuse(format!(
            "expected {} elements in {described}, got {}",
            members.len(),
            items.len(),
        )));
    }
    items
        .iter()
        .zip(members)
        .map(|(item, member)| member_to_cached(item, member, argument))
        .collect()
}

/// One member's own value — recursing for a nested tuple member, so
/// `tuple<a: str, inner: tuple<x: int64, y: int64>>` takes a nested tuple
/// too, and converting anything else the ordinary way.
fn member_to_cached(
    value: &Bound<'_, PyAny>,
    member: &JsonMember,
    argument: &Argument<'_, '_>,
) -> PyResult<DecodedValue> {
    match &member.kind {
        JsonMemberKind::Tuple { type_name, members } => tuple_to_cached(value, members, type_name.as_deref(), argument),
        _ => py_to_cached(value),
    }
}

/// The cast as it was written, for an error message: `default::Point` for a
/// nominal named tuple, `tuple<name: str, value: str>`-ish for a structural
/// one (member types aren't carried this far, so they read as `…`).
fn describe_tuple_type(members: &[JsonMember], type_name: Option<&str>) -> String {
    if let Some(type_name) = type_name {
        return type_name.to_string();
    }
    let members = members
        .iter()
        .map(|m| match &m.key {
            Some(key) => format!("{key}: …"),
            None => "…".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("tuple<{members}>")
}

/// Reconstructs a Python value from `DecodedValue`, structurally equivalent
/// to what the driver decodes — safe to feed into the existing
/// `_decode()`/`_hydrate()` exactly as if it came from a live query,
/// regardless of whether it actually did or came from the cache.
pub(crate) fn cached_to_py<'py>(py: Python<'py>, value: &DecodedValue) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        DecodedValue::Null => py.None().into_bound(py),
        DecodedValue::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        DecodedValue::I64(i) => PyInt::new(py, *i).into_any(),
        DecodedValue::F64(f) => PyFloat::new(py, *f).into_any(),
        // A jsonb number with nothing to say it is a decimal reads as the
        // float it always has (`JsonMemberKind::Decimal` is the other half).
        DecodedValue::JsonNumber(digits) => PyFloat::new(py, digits.parse().unwrap_or(f64::NAN)).into_any(),
        DecodedValue::Str(s) => PyString::new(py, s).into_any(),
        DecodedValue::Bytes(b) => PyBytes::new(py, b).into_any(),
        DecodedValue::Uuid(bytes) => {
            // `UUID(bytes=...)` rather than `UUID(hex)`: the hex path makes
            // Python strip separators and re-parse base 16, where the bytes
            // path is a single `int.from_bytes`. The old code also built the
            // hex string with a `format!` per byte — sixteen allocations for
            // every UUID in a result set.
            let kwargs = PyDict::new(py);
            kwargs.set_item("bytes", PyBytes::new(py, bytes))?;
            py_types(py)?.uuid.bind(py).call((), Some(&kwargs))?
        }
        DecodedValue::Decimal(s) => py_types(py)?.decimal.bind(py).call1((s.as_str(),))?,
        DecodedValue::Array(items) => {
            // A Postgres array reconstructs as a `list` — matching what
            // a Postgres array has always decoded into, since a
            // plain scalar array-typed property (e.g. `Person.tags:
            // pylon.Array[pylon.Str]`) is delivered to the caller as-is,
            // with no further node-based decoding to hide the container
            // type. See `Composite` below for the other case.
            let elems = items
                .iter()
                .map(|v| cached_to_py(py, v))
                .collect::<PyResult<Vec<_>>>()?;
            PyList::new(py, elems)?.into_any()
        }
        DecodedValue::Composite(items) => {
            // A positional composite/record reconstructs as a `tuple`,
            // matching a record row's own behavior — see
            // `py_to_cached`'s note on why `_decode()` needs this distinct
            // from `Array`/`list`.
            let elems = items
                .iter()
                .map(|v| cached_to_py(py, v))
                .collect::<PyResult<Vec<_>>>()?;
            PyTuple::new(py, elems)?.into_any()
        }
        DecodedValue::Object(entries) => {
            let d = PyDict::new(py);
            for (k, v) in entries {
                d.set_item(k, cached_to_py(py, v)?)?;
            }
            d.into_any()
        }
        DecodedValue::Interval {
            months,
            days,
            microseconds,
        } => {
            if *months != 0 {
                // `datetime.timedelta` has no month/year component — a month
                // isn't a fixed span without a date to count it from — so a
                // month-bearing interval keeps its three components apart in
                // `RelativeDuration`, which is what `cal::relative_duration`
                // and `cal::date_duration` both decode into. An interval
                // without months is a `std::duration` and stays a `timedelta`.
                return py_types(py)?
                    .relative_duration
                    .bind(py)
                    .call1((*months, *days, *microseconds));
            }
            // Positional form: timedelta(days, seconds, microseconds, ...).
            py_types(py)?.timedelta.bind(py).call1((*days, 0, *microseconds))?
        }
        DecodedValue::Date(days) => {
            let (y, m, d) = civil_from_days(i64::from(*days) + PG_EPOCH_DAYS_FROM_UNIX);
            PyDate::new(py, y, m, d)?.into_any()
        }
        DecodedValue::Time(us) => {
            // PG `time` is always in [0, 86_400_000_000) microseconds —
            // non-negative, so plain division/remainder suffices.
            let microsecond = (us % 1_000_000) as u32;
            let total_s = us / 1_000_000;
            PyTime::new(
                py,
                (total_s / 3600) as u8,
                ((total_s / 60) % 60) as u8,
                (total_s % 60) as u8,
                microsecond,
                None,
            )?
            .into_any()
        }
        DecodedValue::Timestamp(us) => {
            let (days, microsecond, hour, minute, second) = split_epoch_micros(*us);
            let (y, m, d) = civil_from_days(days);
            PyDateTime::new(py, y, m, d, hour, minute, second, microsecond, None)?.into_any()
        }
        DecodedValue::Timestamptz(us) => {
            let (days, microsecond, hour, minute, second) = split_epoch_micros(*us);
            let (y, m, d) = civil_from_days(days);
            // `PyTzInfo::utc` is itself cached by pyo3, so this is not a
            // per-value import.
            let utc = PyTzInfo::utc(py)?;
            PyDateTime::new(py, y, m, d, hour, minute, second, microsecond, Some(&utc))?.into_any()
        }
        DecodedValue::Range {
            lower,
            upper,
            inc_lower,
            inc_upper,
            empty,
        } => {
            let lower_py = match lower {
                Some(v) => cached_to_py(py, v)?,
                None => py.None().into_bound(py),
            };
            let upper_py = match upper {
                Some(v) => cached_to_py(py, v)?,
                None => py.None().into_bound(py),
            };
            py_types(py)?
                .range
                .bind(py)
                .call1((lower_py, upper_py, *inc_lower, *inc_upper, *empty))?
        }
    })
}

#[cfg(test)]
mod calendar_tests {
    use super::{PG_EPOCH_DAYS_FROM_UNIX, US_PER_DAY, civil_from_days, days_from_civil, split_epoch_micros};

    #[test]
    fn the_two_epochs_are_the_documented_distance_apart() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), PG_EPOCH_DAYS_FROM_UNIX);
    }

    #[test]
    fn round_trips_every_day_across_four_centuries() {
        // Covers all four century-leap cases (1900 and 2100 not leap, 2000
        // leap) and every ordinary leap year in between, which is where a
        // hand-rolled calendar conversion actually breaks.
        for z in days_from_civil(1800, 1, 1)..=days_from_civil(2200, 1, 1) {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(
                days_from_civil(y, m.into(), d.into()),
                z,
                "round trip failed at day {z}"
            );
        }
    }

    #[test]
    fn handles_the_century_leap_rule() {
        // 2000 is a leap year (divisible by 400); 1900 and 2100 are not.
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 28) + 1), (2000, 2, 29));
        assert_eq!(civil_from_days(days_from_civil(1900, 2, 28) + 1), (1900, 3, 1));
        assert_eq!(civil_from_days(days_from_civil(2100, 2, 28) + 1), (2100, 3, 1));
    }

    #[test]
    fn handles_dates_before_the_pylon_epoch() {
        // Negative microsecond counts are the case truncating division gets
        // wrong: 1999-12-31T23:59:59.999999 is one microsecond before the
        // epoch, and must not land on 2000-01-01 or on day -1 at hour 0.
        let (days, us, h, m, s) = split_epoch_micros(-1);
        assert_eq!(civil_from_days(days), (1999, 12, 31));
        assert_eq!((h, m, s, us), (23, 59, 59, 999_999));
    }

    #[test]
    fn splits_a_whole_day_before_the_epoch_exactly() {
        let (days, us, h, m, s) = split_epoch_micros(-US_PER_DAY);
        assert_eq!(civil_from_days(days), (1999, 12, 31));
        assert_eq!((h, m, s, us), (0, 0, 0, 0));
    }

    #[test]
    fn splits_the_epoch_itself() {
        let (days, us, h, m, s) = split_epoch_micros(0);
        assert_eq!(civil_from_days(days), (2000, 1, 1));
        assert_eq!((h, m, s, us), (0, 0, 0, 0));
    }

    #[test]
    fn covers_the_extremes_python_datetime_can_represent() {
        assert_eq!(civil_from_days(days_from_civil(1, 1, 1)), (1, 1, 1));
        assert_eq!(civil_from_days(days_from_civil(9999, 12, 31)), (9999, 12, 31));
    }
}
