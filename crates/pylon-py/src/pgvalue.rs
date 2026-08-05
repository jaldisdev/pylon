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
//! shared decode target both `pylon-cache` (cache hits) and `pylon-pgcon`
//! (fresh rows off the wire) produce. One conversion here means a cache
//! hit and a fresh query result become indistinguishable to Python by the
//! time either reaches `cached_to_py`: the *existing*, unmodified
//! `_decode()`/`_hydrate()` in `pylon/query.py` (driven by
//! `CompiledQuery.shape`) does the real interpretation on both paths.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};

use pylon_value::DecodedValue;

/// Encodes an already-decoded Python value (from asyncpg historically, or
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
    if value.is_instance(&py.import("uuid")?.getattr("UUID")?)? {
        let raw: Vec<u8> = value.getattr("bytes")?.extract()?;
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&raw);
        return Ok(DecodedValue::Uuid(bytes));
    }
    if value.is_instance(&py.import("decimal")?.getattr("Decimal")?)? {
        return Ok(DecodedValue::Decimal(value.str()?.extract()?));
    }
    if value.is_instance(&py.import("datetime")?.getattr("timedelta")?)? {
        // `timedelta` only ever carries days/seconds/microseconds (Python
        // normalizes seconds into days+microseconds internally too) — never
        // months, so this always round-trips through `Interval` with
        // months == 0, matching `std::duration`'s own convention.
        let days: i32 = value.getattr("days")?.extract()?;
        let seconds: i64 = value.getattr("seconds")?.extract()?;
        let microseconds: i64 = value.getattr("microseconds")?.extract()?;
        return Ok(DecodedValue::Interval { months: 0, days, microseconds: seconds * 1_000_000 + microseconds });
    }
    // `datetime.datetime` is a subclass of `datetime.date` — must be checked
    // first, or every datetime would also match the plain-date branch below.
    // Epoch math is delegated to Python's own `datetime` subtraction rather
    // than reimplemented in Rust (proleptic Gregorian calendar arithmetic is
    // exactly what the stdlib already gets right).
    if value.is_instance(&py.import("datetime")?.getattr("datetime")?)? {
        let datetime_cls = py.import("datetime")?.getattr("datetime")?;
        let tzinfo = value.getattr("tzinfo")?;
        let (epoch, target) = if tzinfo.is_none() {
            (datetime_cls.call1((2000, 1, 1))?, value.clone())
        } else {
            // Normalize to UTC first — PostgreSQL's `timestamptz` wire
            // format is always UTC microseconds since the PG epoch,
            // regardless of the value's original tzinfo.
            let utc = py.import("datetime")?.getattr("timezone")?.getattr("utc")?;
            (datetime_cls.call1((2000, 1, 1, 0, 0, 0, 0, &utc))?, value.call_method1("astimezone", (&utc,))?)
        };
        let delta = target.call_method1("__sub__", (epoch,))?;
        let days: i64 = delta.getattr("days")?.extract()?;
        let seconds: i64 = delta.getattr("seconds")?.extract()?;
        let microseconds: i64 = delta.getattr("microseconds")?.extract()?;
        let total_us = days * 86_400_000_000 + seconds * 1_000_000 + microseconds;
        return Ok(if tzinfo.is_none() {
            DecodedValue::Timestamp(total_us)
        } else {
            DecodedValue::Timestamptz(total_us)
        });
    }
    if value.is_instance(&py.import("datetime")?.getattr("date")?)? {
        let epoch = py.import("datetime")?.getattr("date")?.call1((2000, 1, 1))?;
        let delta = value.call_method1("__sub__", (epoch,))?;
        let days: i32 = delta.getattr("days")?.extract()?;
        return Ok(DecodedValue::Date(days));
    }
    if value.is_instance(&py.import("datetime")?.getattr("time")?)? {
        if !value.getattr("tzinfo")?.is_none() {
            return Err(PyValueError::new_err(
                "cannot bind a timezone-aware datetime.time — PostgreSQL `time` (cal::local_time) has no timezone"
            ));
        }
        let hour: i64 = value.getattr("hour")?.extract()?;
        let minute: i64 = value.getattr("minute")?.extract()?;
        let second: i64 = value.getattr("second")?.extract()?;
        let microsecond: i64 = value.getattr("microsecond")?.extract()?;
        let total_us = ((hour * 60 + minute) * 60 + second) * 1_000_000 + microsecond;
        return Ok(DecodedValue::Time(total_us));
    }
    if value.is_instance(&py.import("pylon.datatypes")?.getattr("Range")?)? {
        let empty: bool = value.getattr("empty")?.extract()?;
        let lower = value.getattr("lower")?;
        let upper = value.getattr("upper")?;
        return Ok(DecodedValue::Range {
            lower: if lower.is_none() { None } else { Some(Box::new(py_to_cached(&lower)?)) },
            upper: if upper.is_none() { None } else { Some(Box::new(py_to_cached(&upper)?)) },
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
    // (`Composite`, e.g. `asyncpg.Record`, a plain `tuple`) matter on the
    // way back out: `_decode()`'s `"named_tuple"` case uses `isinstance(_,
    // (dict, list))` to tell "this position already holds the raw jsonb
    // value" apart from "this position holds a composite that needs
    // `value[pos]` indexing first" — `asyncpg.Record` (and, correspondingly,
    // `DecodedValue::Composite` → `tuple`) reads as neither dict nor list,
    // which the check relies on. A `list` genuinely means "Postgres array."
    if let Ok(l) = value.cast::<PyList>() {
        let items = l.iter().map(|item| py_to_cached(&item)).collect::<PyResult<Vec<_>>>()?;
        return Ok(DecodedValue::Array(items));
    }
    if let Ok(len) = value.len() {
        let items = (0..len).map(|i| py_to_cached(&value.get_item(i)?)).collect::<PyResult<Vec<_>>>()?;
        return Ok(DecodedValue::Composite(items));
    }
    Err(PyValueError::new_err(format!(
        "cannot convert a value of type {} to DecodedValue",
        value.get_type().name()?
    )))
}

/// Reconstructs a Python value from `DecodedValue`, structurally equivalent
/// to what asyncpg would have decoded — safe to feed into the existing
/// `_decode()`/`_hydrate()` exactly as if it came from a live query,
/// regardless of whether it actually did or came from the cache.
pub(crate) fn cached_to_py<'py>(py: Python<'py>, value: &DecodedValue) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        DecodedValue::Null => py.None().into_bound(py),
        DecodedValue::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        DecodedValue::I64(i) => PyInt::new(py, *i).into_any(),
        DecodedValue::F64(f) => PyFloat::new(py, *f).into_any(),
        DecodedValue::Str(s) => PyString::new(py, s).into_any(),
        DecodedValue::Bytes(b) => PyBytes::new(py, b).into_any(),
        DecodedValue::Uuid(bytes) => {
            // Passed as a hex string (not a `bytes` kwarg) to avoid needing
            // an extra crate just for keyword-argument construction here.
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            py.import("uuid")?.getattr("UUID")?.call1((hex,))?
        }
        DecodedValue::Decimal(s) => py.import("decimal")?.getattr("Decimal")?.call1((s,))?,
        DecodedValue::Array(items) => {
            // A Postgres array reconstructs as a `list` — matching what
            // asyncpg has always decoded a Postgres array into, since a
            // plain scalar array-typed property (e.g. `Person.tags:
            // pylon.Array[pylon.Str]`) is delivered to the caller as-is,
            // with no further node-based decoding to hide the container
            // type. See `Composite` below for the other case.
            let elems = items.iter().map(|v| cached_to_py(py, v)).collect::<PyResult<Vec<_>>>()?;
            PyList::new(py, elems)?.into_any()
        }
        DecodedValue::Composite(items) => {
            // A positional composite/record reconstructs as a `tuple`,
            // matching `asyncpg.Record`'s own behavior — see
            // `py_to_cached`'s note on why `_decode()` needs this distinct
            // from `Array`/`list`.
            let elems = items.iter().map(|v| cached_to_py(py, v)).collect::<PyResult<Vec<_>>>()?;
            PyTuple::new(py, elems)?.into_any()
        }
        DecodedValue::Object(entries) => {
            let d = PyDict::new(py);
            for (k, v) in entries {
                d.set_item(k, cached_to_py(py, v)?)?;
            }
            d.into_any()
        }
        DecodedValue::Interval { months, days, microseconds } => {
            if *months != 0 {
                // `datetime.timedelta` has no month/year component (a
                // "month" isn't a fixed span without a reference date) —
                // only `std::duration` (months always 0) and the common
                // `cal::relative_duration` calls that don't set years/months
                // decode today; a genuinely month-bearing relative_duration
                // needs a richer Python type this crate doesn't have yet.
                return Err(PyValueError::new_err(
                    "decoding a cal::relative_duration with nonzero years/months \
                     is not yet supported"
                ));
            }
            // Positional form: timedelta(days, seconds, microseconds, ...).
            py.import("datetime")?.getattr("timedelta")?.call1((*days, 0, *microseconds))?
        }
        DecodedValue::Date(days) => {
            let epoch = py.import("datetime")?.getattr("date")?.call1((2000, 1, 1))?;
            let delta = py.import("datetime")?.getattr("timedelta")?.call1((*days,))?;
            epoch.call_method1("__add__", (delta,))?
        }
        DecodedValue::Time(us) => {
            // PG `time` is always in [0, 86_400_000_000) microseconds —
            // non-negative, so plain euclidean division/remainder suffices.
            let microsecond = us.rem_euclid(1_000_000);
            let total_s = us.div_euclid(1_000_000);
            let second = total_s.rem_euclid(60);
            let total_m = total_s.div_euclid(60);
            let minute = total_m.rem_euclid(60);
            let hour = total_m.div_euclid(60);
            py.import("datetime")?.getattr("time")?.call1((hour, minute, second, microsecond))?
        }
        DecodedValue::Timestamp(us) => {
            let epoch = py.import("datetime")?.getattr("datetime")?.call1((2000, 1, 1))?;
            let delta = py.import("datetime")?.getattr("timedelta")?.call1((0, 0, *us))?;
            epoch.call_method1("__add__", (delta,))?
        }
        DecodedValue::Timestamptz(us) => {
            let utc = py.import("datetime")?.getattr("timezone")?.getattr("utc")?;
            let epoch = py.import("datetime")?.getattr("datetime")?.call1((2000, 1, 1, 0, 0, 0, 0, utc))?;
            let delta = py.import("datetime")?.getattr("timedelta")?.call1((0, 0, *us))?;
            epoch.call_method1("__add__", (delta,))?
        }
        DecodedValue::Range { lower, upper, inc_lower, inc_upper, empty } => {
            let lower_py = match lower {
                Some(v) => cached_to_py(py, v)?,
                None => py.None().into_bound(py),
            };
            let upper_py = match upper {
                Some(v) => cached_to_py(py, v)?,
                None => py.None().into_bound(py),
            };
            py.import("pylon.datatypes")?.getattr("Range")?.call1((lower_py, upper_py, *inc_lower, *inc_upper, *empty))?
        }
    })
}
