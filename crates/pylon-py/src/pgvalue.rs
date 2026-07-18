//! Conversion between Python objects and `pylon_value::CachedValue` — the
//! shared decode target both `pylon-cache` (cache hits) and `pylon-pgcon`
//! (fresh rows off the wire) produce. One conversion here means a cache
//! hit and a fresh query result become indistinguishable to Python by the
//! time either reaches `cached_to_py`: the *existing*, unmodified
//! `_decode()`/`_hydrate()` in `pylon/query.py` (driven by
//! `CompiledQuery.shape`) does the real interpretation on both paths.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};

use pylon_value::CachedValue;

/// Encodes an already-decoded Python value (from asyncpg historically, or
/// any caller handing us a plain Python value to bind/cache) into
/// `CachedValue`. Runtime-type-driven, not shape-driven — the shape is
/// only consulted later, by the unmodified `_decode()`.
pub(crate) fn py_to_cached(value: &Bound<'_, PyAny>) -> PyResult<CachedValue> {
    let py = value.py();

    if value.is_none() {
        return Ok(CachedValue::Null);
    }
    // Order matters: `bool` is a subclass of `int` in Python.
    if let Ok(b) = value.cast::<PyBool>() {
        return Ok(CachedValue::Bool(b.is_true()));
    }
    if let Ok(i) = value.cast::<PyInt>() {
        return Ok(CachedValue::I64(i.extract()?));
    }
    if let Ok(f) = value.cast::<PyFloat>() {
        return Ok(CachedValue::F64(f.extract()?));
    }
    if let Ok(s) = value.cast::<PyString>() {
        return Ok(CachedValue::Str(s.extract()?));
    }
    if let Ok(b) = value.cast::<PyBytes>() {
        return Ok(CachedValue::Bytes(b.as_bytes().to_vec()));
    }
    if value.is_instance(&py.import("uuid")?.getattr("UUID")?)? {
        let raw: Vec<u8> = value.getattr("bytes")?.extract()?;
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&raw);
        return Ok(CachedValue::Uuid(bytes));
    }
    if value.is_instance(&py.import("decimal")?.getattr("Decimal")?)? {
        return Ok(CachedValue::Decimal(value.str()?.extract()?));
    }
    if value.is_instance(&py.import("datetime")?.getattr("timedelta")?)? {
        // `timedelta` only ever carries days/seconds/microseconds (Python
        // normalizes seconds into days+microseconds internally too) — never
        // months, so this always round-trips through `Interval` with
        // months == 0, matching `std::duration`'s own convention.
        let days: i32 = value.getattr("days")?.extract()?;
        let seconds: i64 = value.getattr("seconds")?.extract()?;
        let microseconds: i64 = value.getattr("microseconds")?.extract()?;
        return Ok(CachedValue::Interval { months: 0, days, microseconds: seconds * 1_000_000 + microseconds });
    }
    if let Ok(d) = value.cast::<PyDict>() {
        let entries = d
            .iter()
            .map(|(k, v)| Ok((k.extract::<String>()?, py_to_cached(&v)?)))
            .collect::<PyResult<Vec<_>>>()?;
        return Ok(CachedValue::Object(entries));
    }
    // A genuine Postgres array (`Array`) vs. a positional composite/record
    // (`Composite`, e.g. `asyncpg.Record`, a plain `tuple`) matter on the
    // way back out: `_decode()`'s `"named_tuple"` case uses `isinstance(_,
    // (dict, list))` to tell "this position already holds the raw jsonb
    // value" apart from "this position holds a composite that needs
    // `value[pos]` indexing first" — `asyncpg.Record` (and, correspondingly,
    // `CachedValue::Composite` → `tuple`) reads as neither dict nor list,
    // which the check relies on. A `list` genuinely means "Postgres array."
    if let Ok(l) = value.cast::<PyList>() {
        let items = l.iter().map(|item| py_to_cached(&item)).collect::<PyResult<Vec<_>>>()?;
        return Ok(CachedValue::Array(items));
    }
    if let Ok(len) = value.len() {
        let items = (0..len).map(|i| py_to_cached(&value.get_item(i)?)).collect::<PyResult<Vec<_>>>()?;
        return Ok(CachedValue::Composite(items));
    }
    Err(PyValueError::new_err(format!(
        "cannot convert a value of type {} to CachedValue",
        value.get_type().name()?
    )))
}

/// Reconstructs a Python value from `CachedValue`, structurally equivalent
/// to what asyncpg would have decoded — safe to feed into the existing
/// `_decode()`/`_hydrate()` exactly as if it came from a live query,
/// regardless of whether it actually did or came from the cache.
pub(crate) fn cached_to_py<'py>(py: Python<'py>, value: &CachedValue) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        CachedValue::Null => py.None().into_bound(py),
        CachedValue::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        CachedValue::I64(i) => PyInt::new(py, *i).into_any(),
        CachedValue::F64(f) => PyFloat::new(py, *f).into_any(),
        CachedValue::Str(s) => PyString::new(py, s).into_any(),
        CachedValue::Bytes(b) => PyBytes::new(py, b).into_any(),
        CachedValue::Uuid(bytes) => {
            // Passed as a hex string (not a `bytes` kwarg) to avoid needing
            // an extra crate just for keyword-argument construction here.
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            py.import("uuid")?.getattr("UUID")?.call1((hex,))?
        }
        CachedValue::Decimal(s) => py.import("decimal")?.getattr("Decimal")?.call1((s,))?,
        CachedValue::Array(items) => {
            // A Postgres array reconstructs as a `list` — matching what
            // asyncpg has always decoded a Postgres array into, since a
            // plain scalar array-typed property (e.g. `Person.tags:
            // pylon.Array[pylon.Str]`) is delivered to the caller as-is,
            // with no further node-based decoding to hide the container
            // type. See `Composite` below for the other case.
            let elems = items.iter().map(|v| cached_to_py(py, v)).collect::<PyResult<Vec<_>>>()?;
            PyList::new(py, elems)?.into_any()
        }
        CachedValue::Composite(items) => {
            // A positional composite/record reconstructs as a `tuple`,
            // matching `asyncpg.Record`'s own behavior — see
            // `py_to_cached`'s note on why `_decode()` needs this distinct
            // from `Array`/`list`.
            let elems = items.iter().map(|v| cached_to_py(py, v)).collect::<PyResult<Vec<_>>>()?;
            PyTuple::new(py, elems)?.into_any()
        }
        CachedValue::Object(entries) => {
            let d = PyDict::new(py);
            for (k, v) in entries {
                d.set_item(k, cached_to_py(py, v)?)?;
            }
            d.into_any()
        }
        CachedValue::Interval { months, days, microseconds } => {
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
    })
}
