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

//! `/api/...` route handlers.
//!
//! **Known gap, flagged explicitly rather than silently shipped**: the
//! `shape` field `/api/query`'s response carries (driving the frontend's
//! type-aware `JsonTree` rendering — `shape_value_tags`/`_mark_decimals`/
//! decimal-aware shape tagging) is not implemented yet; this returns
//! `"shape": null` for now. Porting it is real, separate work (walking
//! `ShapeNode` into the frontend's value-tree-aligned tag format) — noted
//! here so it isn't mistaken for an oversight.

use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};
use pylon_core::ir::SessionConfig;
use pylon_core::schema::SchemaDescriptor;
use pylon_value::DecodedValue;

use crate::json::json_response;
use crate::state::AppState;
use crate::to_json::{client_error_payload, value_to_json};

fn json_to_cached_value(v: &serde_json::Value) -> DecodedValue {
    match v {
        serde_json::Value::Null => DecodedValue::Null,
        serde_json::Value::Bool(b) => DecodedValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                DecodedValue::I64(i)
            } else {
                DecodedValue::F64(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => DecodedValue::Str(s.clone()),
        serde_json::Value::Array(items) => DecodedValue::Array(items.iter().map(json_to_cached_value).collect()),
        serde_json::Value::Object(map) => {
            DecodedValue::Object(map.iter().map(|(k, v)| (k.clone(), json_to_cached_value(v))).collect())
        }
    }
}

/// One request body's worth of `pyql`/`params`/`globals`/`config` —
/// shared shape for `/api/query`, `/api/analyze`.
struct QueryRequest {
    pyql: String,
    params: Vec<(String, DecodedValue)>,
    globals: Vec<(String, DecodedValue)>,
    config: SessionConfig,
}

fn parse_query_request(body: &serde_json::Value) -> QueryRequest {
    let pyql = body.get("pyql").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let params = body
        .get("params")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), json_to_cached_value(v))).collect())
        .unwrap_or_default();
    let globals = body
        .get("globals")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), json_to_cached_value(v))).collect())
        .unwrap_or_default();
    let allow_user_specified_id = body
        .get("config")
        .and_then(|v| v.get("allow_user_specified_id"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    QueryRequest {
        pyql,
        params,
        globals,
        config: SessionConfig {
            allow_user_specified_id,
        },
    }
}

pub async fn handle_query(state: Arc<AppState>, connection: &str, body: serde_json::Value) -> Response<Full<Bytes>> {
    let client = match state.resolve_client(connection).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return json_response(
                StatusCode::NOT_FOUND,
                &serde_json::json!({"error": format!("No connection named {connection:?}")}),
            );
        }
        Err(e) => return json_response(StatusCode::INTERNAL_SERVER_ERROR, &client_error_payload(&e)),
    };
    let req = parse_query_request(&body);
    let target = client.with_globals(req.globals).with_config(req.config);
    let param_refs: Vec<(&str, DecodedValue)> = req.params.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();

    let start = std::time::Instant::now();
    let objects = match target.query(&req.pyql, &param_refs).await {
        Ok(o) => o,
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &client_error_payload(&e)),
    };
    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;

    json_response(
        StatusCode::OK,
        &serde_json::json!({
            "objects": objects.iter().map(value_to_json).collect::<Vec<_>>(),
            "duration_ms": duration_ms,
            "shape": serde_json::Value::Null,
        }),
    )
}

pub async fn handle_analyze(state: Arc<AppState>, connection: &str, body: serde_json::Value) -> Response<Full<Bytes>> {
    let client = match state.resolve_client(connection).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return json_response(
                StatusCode::NOT_FOUND,
                &serde_json::json!({"error": format!("No connection named {connection:?}")}),
            );
        }
        Err(e) => return json_response(StatusCode::INTERNAL_SERVER_ERROR, &client_error_payload(&e)),
    };
    let req = parse_query_request(&body);
    let target = client.with_globals(req.globals).with_config(req.config);
    let param_refs: Vec<(&str, DecodedValue)> = req.params.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();

    let start = std::time::Instant::now();
    let coarse_grained = match target.analyze(&req.pyql, &param_refs).await {
        Ok(s) => s,
        Err(e) => return json_response(StatusCode::BAD_REQUEST, &client_error_payload(&e)),
    };
    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
    let coarse_grained_json: serde_json::Value =
        serde_json::from_str(&coarse_grained).unwrap_or(serde_json::Value::Null);

    json_response(
        StatusCode::OK,
        &serde_json::json!({"coarse_grained": coarse_grained_json, "duration_ms": duration_ms}),
    )
}

/// `GET /api/<connection>/stats` — excludes junction tables (both `through()`-backed and implicit) from
/// the `pg_stat_user_tables`-derived object-count estimate.
pub async fn handle_stats(state: Arc<AppState>, connection: &str) -> Response<Full<Bytes>> {
    let client = match state.resolve_client(connection).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return json_response(
                StatusCode::NOT_FOUND,
                &serde_json::json!({"error": format!("No connection named {connection:?}")}),
            );
        }
        Err(e) => return json_response(StatusCode::INTERNAL_SERVER_ERROR, &client_error_payload(&e)),
    };
    let schema = client.schema();

    let mut junction_tables: Vec<(String, String)> = Vec::new();
    for t in &schema.types {
        let pg_schema = if t.module == "default" {
            "public".to_string()
        } else {
            t.module.clone()
        };
        if t.junction {
            junction_tables.push((pg_schema.clone(), t.table.clone()));
        }
        for ml in &t.multilinks {
            if ml.through.is_none() {
                junction_tables.push((pg_schema.clone(), format!("{}.{}", t.table, ml.name)));
            }
        }
    }
    junction_tables.sort();

    let junction_filter = if junction_tables.is_empty() {
        String::new()
    } else {
        let excluded = junction_tables
            .iter()
            .map(|(s, t)| format!("('{}', '{}')", s.replace('\'', "''"), t.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        format!("AND (schemaname, relname) NOT IN ({excluded})")
    };

    let sql = format!(
        "SELECT (SUM(n_live_tup)::bigint) AS result FROM pg_stat_user_tables \
         WHERE schemaname NOT IN ('pg_catalog', 'information_schema', '_pylon') {junction_filter}"
    );
    let rows = match client
        .raw_connection()
        .query_typed(&sql, &[], &pylon_pgcon::ExtensionOids::default())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({"error": e.to_string()}),
            );
        }
    };
    let estimated_objects = match rows.first() {
        Some(pylon_value::DecodedValue::I64(n)) => *n,
        _ => 0,
    };

    json_response(
        StatusCode::OK,
        &serde_json::json!({
            "objects": estimated_objects,
            "types": schema.types.len() + schema.scalars.len(),
        }),
    )
}

/// `GET /api/connections` — process-level, no DB round trip.
pub fn handle_connections(state: &AppState) -> Response<Full<Bytes>> {
    let mut others: Vec<&str> = state
        .config
        .connections
        .keys()
        .filter(|k| k.as_str() != "default")
        .map(String::as_str)
        .collect();
    others.sort();
    let mut connections = vec!["main".to_string()];
    connections.extend(others.into_iter().map(String::from));

    json_response(
        StatusCode::OK,
        &serde_json::json!({
            "project": state.config.project.name,
            "connections": connections,
        }),
    )
}

/// `GET /api/models` — only "chat"-purpose models.
pub fn handle_models(state: &AppState) -> Response<Full<Bytes>> {
    let mut models: Vec<serde_json::Value> = state
        .config
        .models
        .iter()
        .filter(|(_, cfg)| cfg.purpose == crate::config::ModelPurpose::Chat)
        .map(|(name, cfg)| {
            serde_json::json!({
                "name": name,
                "model": cfg.model,
                "apiStyle": match cfg.api_style {
                    crate::config::ApiStyle::OpenAi => "openai",
                    crate::config::ApiStyle::Anthropic => "anthropic",
                },
            })
        })
        .collect();
    models.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    json_response(StatusCode::OK, &serde_json::json!({"models": models}))
}

/// `GET /api/config-options` — a small fixed registry, not schema-derived.
pub fn handle_config_options() -> Response<Full<Bytes>> {
    let options: Vec<serde_json::Value> = crate::config_options::CONFIG_OPTIONS
        .iter()
        .map(|o| serde_json::json!({"name": o.name, "typeName": o.type_name, "default": o.default}))
        .collect();
    json_response(StatusCode::OK, &serde_json::json!({"options": options}))
}

/// Schema/globals are process-level (identical regardless of which named
/// connection is selected — the same `_pylon."Schema"` snapshot row backs
/// all of them), so these two routes just need *some* connected `Client` to
/// read `.schema()` off of; the base/"main" connection always exists in
/// `config.connections`.
async fn render_from_any_client(
    state: &AppState,
    render: impl FnOnce(&SchemaDescriptor) -> serde_json::Value,
) -> Response<Full<Bytes>> {
    match state.resolve_client("main").await {
        Ok(Some(c)) => json_response(StatusCode::OK, &render(&c.schema())),
        Ok(None) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({"error": "no [database] connection configured"}),
        ),
        Err(e) => json_response(StatusCode::INTERNAL_SERVER_ERROR, &client_error_payload(&e)),
    }
}

/// `GET /api/schema`.
pub async fn handle_schema(state: Arc<AppState>) -> Response<Full<Bytes>> {
    render_from_any_client(&state, crate::schema_json::schema_json).await
}

/// `GET /api/globals`.
pub async fn handle_globals(state: Arc<AppState>) -> Response<Full<Bytes>> {
    render_from_any_client(&state, crate::schema_json::globals_json).await
}
