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

use super::{FnDescriptor, FnVolatility, ImplStrategy, Param, PylonFnDef, PylonType, SqlLanguage};

use ImplStrategy::{SqlBuiltin as B, SqlExpression as E, SqlOperator as O, TranspilerIntrinsic as I};
use PylonType::{
    Any, AnyOrderable, AnyPoint, Array, BigInt, Bool, Box2D, Box3D, Bytes, Datetime, Decimal, Duration, Float32,
    Float64, Geography, Geometry, Int16, Int32, Int64, Json, LocalDate, LocalDatetime, LocalTime, Multirange, Optional,
    Range, RelativeDuration, Set, Str, Tuple, Uuid, Vector,
};

mod cal_ns;
mod crypto_ns;
mod math_ns;
mod pgvector_ns;
mod postgis_ns;
mod std_ns;
mod sys_ns;

// ── Type helpers ─────────────────────────────────────────────────────────────

fn arr(t: PylonType) -> PylonType {
    Array(Box::new(t))
}
fn set_of(t: PylonType) -> PylonType {
    Set(Box::new(t))
}
fn opt(t: PylonType) -> PylonType {
    Optional(Box::new(t))
}
fn ro(t: PylonType) -> PylonType {
    Range(Box::new(t))
}
fn mr(t: PylonType) -> PylonType {
    Multirange(Box::new(t))
}
fn tup(ts: Vec<PylonType>) -> PylonType {
    Tuple(ts)
}

// ── Param helpers ─────────────────────────────────────────────────────────────

fn p(name: &'static str, ty: PylonType) -> Param {
    Param {
        name,
        ty,
        variadic: false,
    }
}
fn pv(name: &'static str, ty: PylonType) -> Param {
    Param {
        name,
        ty,
        variadic: true,
    }
}

// ── Descriptor helpers ────────────────────────────────────────────────────────

/// Default volatility for a descriptor: a `PylonFunction` already declares
/// one on its `PylonFnDef`, so mirror it rather than letting the two drift.
/// Everything else is immutable unless the entry opts out via `.vol(...)` —
/// the volatile/modifying builtins are a short, explicit list (see
/// `std_ns`/`crypto_ns`/`sys_ns`).
fn default_volatility(impl_: &ImplStrategy) -> FnVolatility {
    match impl_ {
        ImplStrategy::PylonFunction(def) => def.volatility,
        _ => FnVolatility::Immutable,
    }
}

fn f(ns: &'static str, name: &'static str, params: Vec<Param>, ret: PylonType, impl_: ImplStrategy) -> FnDescriptor {
    FnDescriptor {
        namespace: ns,
        name,
        params,
        return_type: ret,
        volatility: default_volatility(&impl_),
        impl_strategy: impl_,
        cast_target: false,
    }
}

fn fc(ns: &'static str, name: &'static str, params: Vec<Param>, ret: PylonType, impl_: ImplStrategy) -> FnDescriptor {
    FnDescriptor {
        namespace: ns,
        name,
        params,
        return_type: ret,
        volatility: default_volatility(&impl_),
        impl_strategy: impl_,
        cast_target: true,
    }
}

impl FnDescriptor {
    /// Override the derived volatility — for `SqlBuiltin`/`SqlExpression`
    /// entries that wrap a non-immutable PostgreSQL function and so can't
    /// have it inferred from a `PylonFnDef`.
    fn vol(mut self, v: FnVolatility) -> Self {
        self.volatility = v;
        self
    }
}

// ── PylonFnDef helpers ────────────────────────────────────────────────────────

fn sql(name: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name,
        language: SqlLanguage::Sql,
        volatility: FnVolatility::Immutable,
        strict: true,
        returns_override: None,
        body,
    })
}

fn sql_returns(name: &'static str, returns: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name,
        language: SqlLanguage::Sql,
        volatility: FnVolatility::Immutable,
        strict: true,
        returns_override: Some(returns),
        body,
    })
}

fn plpgsql(name: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name,
        language: SqlLanguage::PlPgSql,
        volatility: FnVolatility::Immutable,
        strict: true,
        returns_override: None,
        body,
    })
}

/// PL/pgSQL STABLE, NOT STRICT — for overloads where an optional `msg` param
/// may legitimately be NULL when the caller omits it.
fn plpgsql_stable_nullable(name: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name,
        language: SqlLanguage::PlPgSql,
        volatility: FnVolatility::Stable,
        strict: false,
        returns_override: Some("anyarray"),
        body,
    })
}

/// PL/pgSQL STABLE, NOT STRICT, returning `boolean` (for assert 2-arg).
fn plpgsql_stable_nullable_bool(name: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name,
        language: SqlLanguage::PlPgSql,
        volatility: FnVolatility::Stable,
        strict: false,
        returns_override: Some("boolean"),
        body,
    })
}

/// PL/pgSQL STABLE, NOT STRICT, returning `anyelement` (for assert_single 2-arg).
fn plpgsql_stable_nullable_elem(name: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name,
        language: SqlLanguage::PlPgSql,
        volatility: FnVolatility::Stable,
        strict: false,
        returns_override: Some("anyelement"),
        body,
    })
}

fn plpgsql_stable_returns(name: &'static str, returns: &'static str, body: &'static str) -> ImplStrategy {
    ImplStrategy::PylonFunction(PylonFnDef {
        name,
        language: SqlLanguage::PlPgSql,
        volatility: FnVolatility::Stable,
        strict: true,
        returns_override: Some(returns),
        body,
    })
}

// ── Registry ──────────────────────────────────────────────────────────────────

/// One namespace per file — `std_ns`/`math_ns`/`cal_ns`/`sys_ns` (core
/// stdlib), `pgvector_ns`/`crypto_ns`/`postgis_ns` (extension-backed
/// namespaces, each a thin passthrough to the matching PostgreSQL
/// extension). Concatenated here so `super::registry()` sees one flat list.
pub(super) fn build() -> Vec<FnDescriptor> {
    let mut v = Vec::new();
    v.extend(std_ns::build());
    v.extend(math_ns::build());
    v.extend(cal_ns::build());
    v.extend(sys_ns::build());
    v.extend(pgvector_ns::build());
    v.extend(crypto_ns::build());
    v.extend(postgis_ns::build());
    v
}
