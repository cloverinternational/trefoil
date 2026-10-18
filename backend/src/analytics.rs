//! Semantic analytics and shared saved dashboard endpoints.
use crate::{
    auth::{resolve_principal, Principal},
    AppState,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_RANGE_MS: i64 = 366 * 24 * 60 * 60 * 1000;
const MAX_FILTERS: usize = 16;
const MAX_FILTER_VALUE: usize = 512;
const MAX_FORMULA: usize = 256;
const MAX_CONFIG_BYTES: usize = 64 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v0/management/analytics/query", axum::routing::post(query))
        .route(
            "/v0/management/analytics/dashboards",
            get(list).post(create),
        )
        .route(
            "/v0/management/analytics/dashboards/:id",
            get(read).put(update).delete(remove),
        )
}
fn principal(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    let key = headers.get("x-admin-key").and_then(|v| v.to_str().ok());
    resolve_principal(state, key).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Unauthorized"})),
        )
            .into_response()
    })
}
fn admin(p: &Principal) -> bool {
    matches!(p, Principal::Admin)
}
fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"error":msg}))).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Query {
    start: i64,
    end: i64,
    dimension: String,
    metric: String,
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    filters: Vec<Filter>,
    #[serde(default)]
    formula: Option<String>,
}
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Filter {
    dimension: String,
    value: String,
}
fn dimension(name: &str) -> Option<&'static str> {
    match name {
        "model" => Some("incoming_model_alias"),
        "provider" => Some("provider"),
        "apiKey" => Some("api_key"),
        "permissionGroup" => Some("permission_group"),
        _ => None,
    }
}
fn metric(name: &str) -> Option<&'static str> {
    match name{"requests"=>Some("count(*)::double precision"),"inputTokens"=>Some("coalesce(sum(tokens_input),0)::double precision"),"outputTokens"=>Some("coalesce(sum(tokens_output),0)::double precision"),"tokens"=>Some("sum(coalesce(tokens_input,0)+coalesce(tokens_output,0))::double precision"),"cost"=>Some("coalesce(sum(cost_total),0)::double precision"),"errors"=>Some("count(*) FILTER (WHERE response_status IS NULL OR response_status NOT IN ('success','pending') AND response_status !~ '^2[0-9][0-9]$')::double precision"),"avgTps"=>Some("avg(tokens_per_sec)"),"avgTtftMs"=>Some("avg(ttft_ms)"),"avgDurationMs"=>Some("avg(duration_ms)::double precision"),_=>None}
}
fn formula_valid(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_FORMULA {
        return false;
    }
    let b = s.as_bytes();
    let (mut i, mut expect_value, mut depth, mut terms) = (0, true, 0i32, 0usize);
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || c == b'.' {
            if !expect_value {
                return false;
            }
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1
            }
            if i - start > 32
                || std::str::from_utf8(&b[start..i])
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok())
                    .is_none()
            {
                return false;
            }
            expect_value = false;
            terms += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            if !expect_value {
                return false;
            }
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1
            }
            if !matches!(
                &s[start..i],
                "requests"
                    | "inputTokens"
                    | "outputTokens"
                    | "tokens"
                    | "cost"
                    | "errors"
                    | "avgTps"
                    | "avgTtftMs"
                    | "avgDurationMs"
            ) {
                return false;
            }
            expect_value = false;
            terms += 1;
            continue;
        }
        match c {
            b'(' => {
                if !expect_value && i > 0 {
                    return false;
                }
                depth += 1;
                if depth > 16 {
                    return false;
                }
                expect_value = true
            }
            b')' => {
                if expect_value || depth == 0 {
                    return false;
                }
                depth -= 1;
                expect_value = false
            }
            b'+' | b'-' | b'*' | b'/' => {
                if expect_value {
                    return false;
                }
                expect_value = true
            }
            _ => return false,
        }
        i += 1;
    }
    terms > 0 && !expect_value && depth == 0
}
fn formula_sql(s: &str) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1
            }
            out.push_str(metric(&s[start..i]).unwrap_or("NULL"));
        } else {
            out.push(b[i] as char);
            i += 1
        }
    }
    out
}
fn checked_query(q: &Query) -> Result<(), &'static str> {
    let now = chrono::Utc::now().timestamp_millis();
    if q.start < 0 || q.end <= q.start || q.end - q.start > MAX_RANGE_MS || q.end > now {
        return Err("Invalid time range");
    }
    if dimension(&q.dimension).is_none() && q.dimension != "time" {
        return Err("Invalid dimension");
    }
    if metric(&q.metric).is_none() {
        return Err("Invalid metric");
    }
    if q.filters.len() > MAX_FILTERS
        || q.filters
            .iter()
            .any(|f| dimension(&f.dimension).is_none() || f.value.len() > MAX_FILTER_VALUE)
    {
        return Err("Invalid filters");
    }
    if q.bucket
        .as_deref()
        .is_some_and(|b| !matches!(b, "hour" | "day" | "week" | "month"))
    {
        return Err("Invalid bucket");
    }
    if q.dimension == "time" && q.bucket.is_none() {
        return Err("Time dimension requires bucket");
    }
    if q.formula.as_ref().is_some_and(|f| !formula_valid(f)) {
        return Err("Invalid formula");
    }
    Ok(())
}
async fn query(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(q): Json<Query>,
) -> Response {
    let p = match principal(&state, &headers) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let Err(e) = checked_query(&q) {
        return err(StatusCode::BAD_REQUEST, e);
    }
    let Some(pool) = state.inner.database.as_ref() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable");
    };
    let mut conditions = vec![
        "start_time >= $1".to_string(),
        "start_time < $2".to_string(),
    ];
    let mut bind_count = 2usize;
    if let Principal::Limited { key_name: _, .. } = &p {
        bind_count += 1;
        conditions.push(format!("api_key = ${bind_count}"))
    }
    for f in &q.filters {
        let col = dimension(&f.dimension).unwrap();
        bind_count += 1;
        let n = bind_count;
        if f.value == "Unknown" || f.value == "ungrouped" {
            conditions.push(format!("({col} = ${n} OR {col} IS NULL OR {col} = '')"))
        } else {
            conditions.push(format!("coalesce({col}, 'Unknown') = ${n}"))
        }
    }
    let (group, bucket_ms) = if q.dimension == "time" {
        let ms = match q.bucket.as_deref().unwrap() {
            "hour" => 3_600_000i64,
            "day" => 86_400_000,
            "week" => 604_800_000,
            "month" => 2_592_000_000,
            _ => unreachable!(),
        };
        (format!("floor(start_time / {ms}) * {ms}"), Some(ms))
    } else {
        (
            format!("coalesce({}, 'Unknown')", dimension(&q.dimension).unwrap()),
            None,
        )
    };
    let metric_sql = q
        .formula
        .as_deref()
        .map(formula_sql)
        .unwrap_or_else(|| metric(&q.metric).unwrap().to_string());
    let samples_sql = match q.metric.as_str() {
        "avgTps" => "count(tokens_per_sec)",
        "avgTtftMs" => "count(ttft_ms)",
        "avgDurationMs" => "count(duration_ms)",
        _ => "count(*)",
    };
    let sql=format!("SELECT {group} AS label, {metric_sql} AS value, {samples_sql}::bigint AS samples FROM request_usage WHERE {} GROUP BY label ORDER BY label LIMIT 500",conditions.join(" AND "));
    let rows = match run_query(pool, &sql, q.start, q.end, &p, &q.filters).await {
        Ok(rows) => rows,
        Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "Analytics query failed"),
    };
    let data:Vec<Value>=rows.into_iter().map(|r|{let label=r.try_get::<Option<i64>,_>("label").ok().flatten().map(|n|n.to_string()).or_else(||r.try_get::<Option<String>,_>("label").ok().flatten());let value=r.try_get::<Option<f64>,_>("value").ok().flatten();let samples=r.try_get::<i64,_>("samples").unwrap_or(0);json!({"label":label,"value":value,"samples":samples,"bucketStartMs":bucket_ms.and_then(|_|label.as_ref().and_then(|v|v.parse::<i64>().ok()))})}).collect();
    Json(json!({"data":data,"unit":match q.metric.as_str(){"requests"|"errors"=>"requests","inputTokens"|"outputTokens"|"tokens"=>"tokens","cost"=>"cost","avgTps"=>"tokens/second","avgTtftMs"|"avgDurationMs"=>"milliseconds",_=>""}})).into_response()
}
async fn run_query(
    pool: &sqlx::PgPool,
    sql: &str,
    start: i64,
    end: i64,
    p: &Principal,
    filters: &[Filter],
) -> Result<Vec<sqlx::postgres::PgRow>, sqlx::Error> {
    let mut q = sqlx::query(sql).bind(start).bind(end);
    if let Principal::Limited { key_name, .. } = p {
        q = q.bind(key_name)
    }
    for f in filters {
        q = q.bind(&f.value)
    }
    q.fetch_all(pool).await
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Dashboard {
    id: Option<String>,
    name: String,
    config: Value,
    #[serde(default)]
    version: Option<i64>,
}
fn valid_formula_value(v: &Value) -> bool {
    v.as_str().is_none_or(formula_valid)
}
fn valid(d: &Dashboard) -> bool {
    if d.name.trim().is_empty()
        || d.name.len() > 120
        || !d.config.is_object()
        || d.config.to_string().len() > MAX_CONFIG_BYTES
    {
        return false;
    }
    let Some(panels) = d.config.get("panels").and_then(Value::as_array) else {
        return false;
    };
    if panels.len() > 40 {
        return false;
    }
    let mut ids = HashSet::new();
    panels.iter().all(|p| {
        let id = p.get("id").and_then(Value::as_str);
        let metric_name = p.get("metric").and_then(Value::as_str);
        let dim = p.get("dimension").and_then(Value::as_str);
        id.is_some_and(|x| !x.is_empty() && x.len() <= 80 && ids.insert(x.to_owned()))
            && metric_name.and_then(metric).is_some()
            && dim.is_some_and(|x| dimension(x).is_some() || x == "time")
            && p.get("formula").is_none_or(valid_formula_value)
            && p.get("title")
                .is_none_or(|v| v.as_str().is_some_and(|s| s.len() <= 120))
    })
}
fn db(s: &AppState) -> Result<&sqlx::PgPool, Response> {
    s.inner
        .database
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "Database unavailable"))
}
fn row_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({"id":r.get::<String,_>("id"),"name":r.get::<String,_>("name"),"config":r.get::<Value,_>("config"),"version":r.get::<i64,_>("version"),"createdAt":r.get::<i64,_>("created_at"),"updatedAt":r.get::<i64,_>("updated_at")})
}
async fn list(State(s): State<AppState>, h: HeaderMap) -> Response {
    if let Err(r) = principal(&s, &h) {
        return r;
    }
    let pool = match db(&s) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match sqlx::query("SELECT id,name,config,version,created_at,updated_at FROM saved_dashboards ORDER BY updated_at DESC LIMIT 100").fetch_all(pool).await{Ok(rows)=>Json(Value::Array(rows.iter().map(row_json).collect())).into_response(),Err(_)=>err(StatusCode::INTERNAL_SERVER_ERROR,"Database error")}
}
async fn read(Path(id): Path<String>, State(s): State<AppState>, h: HeaderMap) -> Response {
    if let Err(r) = principal(&s, &h) {
        return r;
    }
    let pool = match db(&s) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match sqlx::query(
        "SELECT id,name,config,version,created_at,updated_at FROM saved_dashboards WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(r)) => Json(row_json(&r)).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "Dashboard not found"),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "Database error"),
    }
}
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
static DASHBOARD_ID: AtomicU64 = AtomicU64::new(0);
async fn create(State(s): State<AppState>, h: HeaderMap, Json(d): Json<Dashboard>) -> Response {
    let p = match principal(&s, &h) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !admin(&p) {
        return err(StatusCode::FORBIDDEN, "Admin required");
    }
    if !valid(&d) {
        return err(StatusCode::BAD_REQUEST, "Invalid dashboard");
    }
    let pool = match db(&s) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let t = now();
    let id = format!("d-{}-{}", t, DASHBOARD_ID.fetch_add(1, Ordering::Relaxed));
    match sqlx::query("INSERT INTO saved_dashboards(id,name,config,created_at,updated_at) VALUES($1,$2,$3,$4,$4) RETURNING version").bind(&id).bind(d.name).bind(d.config).bind(t).fetch_one(pool).await{Ok(r)=>Json(json!({"id":id,"version":r.get::<i64,_>("version")})).into_response(),Err(_)=>err(StatusCode::INTERNAL_SERVER_ERROR,"Database error")}
}
async fn update(
    Path(id): Path<String>,
    State(s): State<AppState>,
    h: HeaderMap,
    Json(d): Json<Dashboard>,
) -> Response {
    let p = match principal(&s, &h) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !admin(&p) {
        return err(StatusCode::FORBIDDEN, "Admin required");
    }
    if !valid(&d) {
        return err(StatusCode::BAD_REQUEST, "Invalid dashboard");
    }
    let Some(v) = d.version.filter(|v| *v > 0) else {
        return err(StatusCode::BAD_REQUEST, "Version required");
    };
    let pool = match db(&s) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match sqlx::query("UPDATE saved_dashboards SET name=$1,config=$2,version=version+1,updated_at=$3 WHERE id=$4 AND version=$5 RETURNING version").bind(d.name).bind(d.config).bind(now()).bind(&id).bind(v).fetch_optional(pool).await{Ok(Some(r))=>Json(json!({"id":id,"version":r.get::<i64,_>("version")})).into_response(),Ok(None)=>err(StatusCode::CONFLICT,"Version conflict or missing dashboard"),Err(_)=>err(StatusCode::INTERNAL_SERVER_ERROR,"Database error")}
}
async fn remove(Path(id): Path<String>, State(s): State<AppState>, h: HeaderMap) -> Response {
    let p = match principal(&s, &h) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !admin(&p) {
        return err(StatusCode::FORBIDDEN, "Admin required");
    }
    let pool = match db(&s) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match sqlx::query("DELETE FROM saved_dashboards WHERE id=$1")
        .bind(id)
        .execute(pool)
        .await
    {
        Ok(r) if r.rows_affected() > 0 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => err(StatusCode::NOT_FOUND, "Dashboard not found"),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "Database error"),
    }
}
