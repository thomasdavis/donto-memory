//! GET /jobs — HTML observability page.
//! GET /jobs.json — JSON list.
//! GET /jobs/:id — HTML detail of a single audit row.
//! GET /jobs/:id.json — raw JSON for a single audit row.
//!
//! Reads `donto_x_memory_job_log` (populated by the memorize / recall /
//! ingest handlers via `job_log::record_job`).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::api::AppState;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub holder: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}
fn default_limit() -> i64 {
    100
}

#[derive(Debug, serde::Serialize)]
struct JobListRow {
    job_id: Uuid,
    created_at: DateTime<Utc>,
    endpoint: String,
    holder: Option<String>,
    session_id: Option<String>,
    status_code: i32,
    elapsed_ms: i64,
    facts_extracted: Option<i32>,
    facts_ingested: Option<i32>,
    rows_returned: Option<i32>,
    model: Option<String>,
    total_tokens: Option<i32>,
    error: Option<String>,
}

async fn fetch_list(
    s: &Arc<AppState>,
    q: &ListQuery,
) -> Result<Vec<JobListRow>, String> {
    let client = s.pool.get().await.map_err(|e| e.to_string())?;
    let limit = q.limit.clamp(1, 1000);
    let rows = client
        .query(
            "select job_id, created_at, endpoint, holder, session_id,
                    status_code, elapsed_ms,
                    facts_extracted, facts_ingested, rows_returned,
                    model, total_tokens, error
               from donto_x_memory_job_log
              where ($1::text is null or endpoint ilike '%' || $1 || '%')
                and ($2::text is null or holder = $2)
              order by created_at desc
              limit $3",
            &[&q.endpoint, &q.holder, &limit],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| JobListRow {
            job_id: r.get("job_id"),
            created_at: r.get("created_at"),
            endpoint: r.get("endpoint"),
            holder: r.get("holder"),
            session_id: r.get("session_id"),
            status_code: r.get("status_code"),
            elapsed_ms: r.get("elapsed_ms"),
            facts_extracted: r.get("facts_extracted"),
            facts_ingested: r.get("facts_ingested"),
            rows_returned: r.get("rows_returned"),
            model: r.get("model"),
            total_tokens: r.get("total_tokens"),
            error: r.get("error"),
        })
        .collect())
}

pub async fn list_json(
    State(s): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Response {
    match fetch_list(&s, &q).await {
        Ok(rows) => Json(json!({
            "count": rows.len(),
            "jobs": rows,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e})),
        )
            .into_response(),
    }
}

pub async fn list_html(
    State(s): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Response {
    match fetch_list(&s, &q).await {
        Ok(rows) => Html(render_list_html(&rows, &q)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("<pre>{}</pre>", html_escape(&e))),
        )
            .into_response(),
    }
}

#[derive(Debug, serde::Serialize)]
struct JobDetail {
    job_id: Uuid,
    created_at: DateTime<Utc>,
    endpoint: String,
    holder: Option<String>,
    session_id: Option<String>,
    status_code: i32,
    elapsed_ms: i64,
    request: Value,
    response: Value,
    facts_extracted: Option<i32>,
    facts_ingested: Option<i32>,
    rows_returned: Option<i32>,
    model: Option<String>,
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
    total_tokens: Option<i32>,
    error: Option<String>,
}

async fn fetch_detail(s: &Arc<AppState>, id: Uuid) -> Result<Option<JobDetail>, String> {
    let client = s.pool.get().await.map_err(|e| e.to_string())?;
    let row = client
        .query_opt(
            "select job_id, created_at, endpoint, holder, session_id,
                    status_code, elapsed_ms, request, response,
                    facts_extracted, facts_ingested, rows_returned,
                    model, prompt_tokens, completion_tokens, total_tokens,
                    error
               from donto_x_memory_job_log
              where job_id = $1",
            &[&id],
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(row.map(|r| JobDetail {
        job_id: r.get("job_id"),
        created_at: r.get("created_at"),
        endpoint: r.get("endpoint"),
        holder: r.get("holder"),
        session_id: r.get("session_id"),
        status_code: r.get("status_code"),
        elapsed_ms: r.get("elapsed_ms"),
        request: r.get::<_, Value>("request"),
        response: r.get::<_, Value>("response"),
        facts_extracted: r.get("facts_extracted"),
        facts_ingested: r.get("facts_ingested"),
        rows_returned: r.get("rows_returned"),
        model: r.get("model"),
        prompt_tokens: r.get("prompt_tokens"),
        completion_tokens: r.get("completion_tokens"),
        total_tokens: r.get("total_tokens"),
        error: r.get("error"),
    }))
}

pub async fn detail_json(
    State(s): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Response {
    match fetch_detail(&s, id).await {
        Ok(Some(d)) => Json(d).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such job"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e})),
        )
            .into_response(),
    }
}

pub async fn detail_html(
    State(s): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Response {
    match fetch_detail(&s, id).await {
        Ok(Some(d)) => Html(render_detail_html(&d)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Html(format!("<h1>job not found</h1><p>{}</p>", html_escape(&id.to_string()))),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("<pre>{}</pre>", html_escape(&e))),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------
// HTML rendering — hand-rolled, no template engine. Same look as the
// homepage CSS palette.

fn html_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn json_pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn page_chrome(title: &str, body: &str) -> String {
    format!(r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>{title} · donto-memory</title>
  <style>
    :root {{
      --fg: #1a1a1a; --fg-soft: #4a4a4a; --muted: #6f6f6f;
      --bg: #fafaf7; --bg-alt: #f4f0e6;
      --link: #1a4fb5; --rule: #d8d4ca;
      --code-bg: #f0ece2; --accent: #6b3fb0;
      --accent-soft: #efeaf5;
      --green: #2f7d3a; --red: #b53935; --orange: #c46b0a;
    }}
    * {{ box-sizing: border-box; }}
    body {{
      font-family: ui-monospace, "SF Mono", "Menlo", "Consolas", monospace;
      font-size: 13px; line-height: 1.5; color: var(--fg);
      background: var(--bg); margin: 0; padding: 0;
    }}
    main {{ max-width: 1280px; margin: 0 auto; padding: 1.4rem 1.5rem 5rem; }}
    header.site {{
      border-bottom: 1px solid var(--rule); padding: 0.7rem 1.5rem;
      font-size: 12px; color: var(--muted);
      display: flex; justify-content: space-between; flex-wrap: wrap; gap: 0.5rem;
      background: var(--bg); position: sticky; top: 0; z-index: 10;
    }}
    header.site .links a {{ color: var(--muted); margin-left: 1rem; }}
    header.site .links a:hover {{ color: var(--fg); }}
    h1 {{ font-size: 1.5rem; font-weight: 700; margin: 0.3rem 0 0.4rem; }}
    h1 .accent {{ color: var(--accent); }}
    h2 {{ font-size: 1.05rem; margin: 2rem 0 0.6rem;
          padding-bottom: 0.25rem; border-bottom: 1px solid var(--rule); }}
    a {{ color: var(--link); text-decoration: none; }}
    a:hover {{ text-decoration: underline; }}
    code {{ background: var(--code-bg); padding: 0.05rem 0.3rem;
            border-radius: 3px; font-size: 0.95em; }}
    pre {{
      background: var(--code-bg); padding: 0.7rem 0.9rem; border-radius: 4px;
      overflow: auto; font-size: 0.86em; line-height: 1.45;
      max-height: 540px; white-space: pre-wrap; word-break: break-word;
    }}
    table {{ border-collapse: collapse; width: 100%; font-size: 0.92em; }}
    th, td {{ border-bottom: 1px solid var(--rule);
             padding: 0.32rem 0.6rem 0.32rem 0; text-align: left;
             vertical-align: top; }}
    th {{ font-weight: 600; color: var(--fg-soft); }}
    tr:hover td {{ background: var(--bg-alt); }}
    .status-200 {{ color: var(--green); font-weight: 600; }}
    .status-400 {{ color: var(--orange); font-weight: 600; }}
    .status-500 {{ color: var(--red); font-weight: 600; }}
    .pill {{ display: inline-block; padding: 0.05rem 0.35rem;
             border-radius: 3px; background: var(--accent-soft);
             color: var(--accent); font-size: 0.85em; }}
    .pill-blue {{ background: #dbe6f5; color: var(--link); }}
    .num {{ text-align: right; font-variant-numeric: tabular-nums; }}
    nav.filters {{ background: var(--bg-alt); border: 1px solid var(--rule);
                   border-radius: 4px; padding: 0.6rem 0.9rem;
                   margin: 0.4rem 0 1rem; font-size: 0.9em; }}
    .meta {{ color: var(--muted); font-size: 0.9em; margin-bottom: 1rem; }}
    .grid {{ display: grid; grid-template-columns: 1fr 1fr; gap: 1rem; }}
    @media (max-width: 980px) {{ .grid {{ grid-template-columns: 1fr; }} }}
    .card {{ border: 1px solid var(--rule); border-radius: 4px;
             padding: 0.7rem 0.9rem; background: #fff; }}
    .card h3 {{ margin: 0 0 0.4rem; font-size: 0.85em;
                text-transform: uppercase; letter-spacing: 0.05em;
                color: var(--muted); }}
    .tab-row {{ display: flex; gap: 0.5rem; margin-bottom: 0.6rem; }}
    .tab-row a {{ padding: 0.2rem 0.55rem; border: 1px solid var(--rule);
                  border-radius: 3px; color: var(--fg-soft);
                  background: #fff; }}
    .tab-row a:hover {{ background: var(--accent-soft); border-color: var(--accent);
                        color: var(--accent); text-decoration: none; }}
    .ago {{ color: var(--muted); font-size: 0.85em; }}
  </style>
</head>
<body>
  <header class="site">
    <span><strong>memories.apexpots.com</strong> · jobs · contract <code>0.1.0-m10</code></span>
    <span class="links">
      <a href="/">home</a>
      <a href="/jobs">jobs</a>
      <a href="/docs">docs</a>
      <a href="/agent.md">agent guide</a>
      <a href="/openapi.json">openapi</a>
      <a href="https://github.com/thomasdavis/donto-memory">github</a>
    </span>
  </header>
  <main>
    {body}
  </main>
</body>
</html>"#)
}

fn status_class(code: i32) -> &'static str {
    if code >= 500 {
        "status-500"
    } else if code >= 400 {
        "status-400"
    } else {
        "status-200"
    }
}

fn render_list_html(rows: &[JobListRow], q: &ListQuery) -> String {
    let mut body = String::new();
    body.push_str(r#"<h1><span class="accent">jobs</span> &nbsp;<small style="color:var(--muted);font-weight:400;font-size:0.8em">every memorize + recall + ingest call</small></h1>"#);

    body.push_str(r#"<p class="meta">Every <code>POST /memorize</code>, <code>POST /recall</code>, and <code>POST /ingest</code> call is logged with full request + response bodies. Click a row for the per-job detail including every extracted fact.</p>"#);

    // filters
    body.push_str(r#"<nav class="filters">filter: "#);
    body.push_str(&format!(
        r#"<form method="get" action="/jobs" style="display:inline">
              <label>endpoint <input name="endpoint" value="{}" placeholder="memorize / recall / ingest" size="22"></label>
              &nbsp;
              <label>holder <input name="holder" value="{}" placeholder="agent:my-bot" size="22"></label>
              &nbsp;
              <label>limit <input name="limit" value="{}" size="5"></label>
              &nbsp;
              <button type="submit">apply</button>
              &nbsp;<a href="/jobs">reset</a>
              &nbsp;<a href="/jobs/list.json">json</a>
           </form>"#,
        html_escape(q.endpoint.as_deref().unwrap_or("")),
        html_escape(q.holder.as_deref().unwrap_or("")),
        q.limit,
    ));
    body.push_str(r#"</nav>"#);

    if rows.is_empty() {
        body.push_str(r#"<p style="margin-top:2rem;color:var(--muted)">No jobs match this filter yet. Make a call to <code>POST /memorize</code> or <code>POST /recall</code> and refresh.</p>"#);
    } else {
        body.push_str(&format!(r#"<p class="meta">{} job(s) shown.</p>"#, rows.len()));

        body.push_str(r#"<table>
<thead><tr>
  <th>when</th>
  <th>endpoint</th>
  <th>holder · session</th>
  <th>status</th>
  <th class="num">ms</th>
  <th class="num">facts</th>
  <th class="num">rows</th>
  <th class="num">tokens</th>
  <th>model</th>
  <th>job</th>
</tr></thead><tbody>"#);
        for r in rows {
            let when = r.created_at.format("%Y-%m-%d %H:%M:%S").to_string();
            let holder_session = match (&r.holder, &r.session_id) {
                (Some(h), Some(s)) => format!("{} · <span style='color:var(--muted)'>{}</span>", html_escape(h), html_escape(s)),
                (Some(h), None) => html_escape(h),
                _ => "<span style='color:var(--muted)'>—</span>".to_string(),
            };
            let facts = match (r.facts_extracted, r.facts_ingested) {
                (Some(e), Some(i)) if e == i => format!("{e}"),
                (Some(e), Some(i)) => format!("{i}/{e}"),
                (Some(e), None) => format!("{e}"),
                _ => "—".to_string(),
            };
            let rows_back = r.rows_returned.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string());
            let toks = r.total_tokens.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string());
            let model = r.model.as_deref().unwrap_or("—");
            body.push_str(&format!(
                r#"<tr>
                  <td><span class="ago">{when}</span></td>
                  <td><code>{}</code></td>
                  <td>{}</td>
                  <td class="{}">{}</td>
                  <td class="num">{}</td>
                  <td class="num">{}</td>
                  <td class="num">{}</td>
                  <td class="num">{}</td>
                  <td>{}</td>
                  <td><a href="/jobs/{}">view →</a></td>
                </tr>"#,
                html_escape(&r.endpoint),
                holder_session,
                status_class(r.status_code), r.status_code,
                r.elapsed_ms,
                facts, rows_back, toks,
                html_escape(model),
                r.job_id,
            ));
        }
        body.push_str("</tbody></table>");
    }

    page_chrome("Jobs", &body)
}

fn render_detail_html(d: &JobDetail) -> String {
    let when = d.created_at.format("%Y-%m-%d %H:%M:%S UTC").to_string();
    let mut body = String::new();
    body.push_str(&format!(
        r#"<h1><span class="accent">job</span> <code>{}</code></h1>
           <p class="meta">{} · <code>{}</code></p>"#,
        d.job_id,
        when,
        html_escape(&d.endpoint),
    ));

    body.push_str(r#"<div class="tab-row">"#);
    body.push_str(&format!(r#"<a href="/jobs">← back to list</a>"#));
    body.push_str(&format!(r#"<a href="/jobs/{}/raw">raw JSON</a>"#, d.job_id));
    if let Some(h) = &d.holder {
        body.push_str(&format!(r#"<a href="/jobs?holder={}">holder: {}</a>"#, urlencode(h), html_escape(h)));
    }
    body.push_str(r#"</div>"#);

    // Summary card
    let status_cls = status_class(d.status_code);
    let facts_cell = match (d.facts_extracted, d.facts_ingested) {
        (Some(e), Some(i)) if e == i => format!("{e}"),
        (Some(e), Some(i)) => format!("{i} ingested / {e} extracted"),
        (Some(e), None) => format!("{e}"),
        _ => "—".to_string(),
    };
    body.push_str(&format!(r#"<table style="margin-bottom:1.5rem">
<tr><th>endpoint</th><td><code>{}</code></td>
    <th>status</th><td class="{status_cls}">{}</td></tr>
<tr><th>holder</th><td><code>{}</code></td>
    <th>session</th><td><code>{}</code></td></tr>
<tr><th>elapsed</th><td>{} ms</td>
    <th>model</th><td>{}</td></tr>
<tr><th>facts</th><td>{}</td>
    <th>rows returned</th><td>{}</td></tr>
<tr><th>prompt tokens</th><td>{}</td>
    <th>completion tokens</th><td>{}</td></tr>
<tr><th>total tokens</th><td>{}</td>
    <th>error</th><td>{}</td></tr>
</table>"#,
        html_escape(&d.endpoint), d.status_code,
        html_escape(d.holder.as_deref().unwrap_or("—")),
        html_escape(d.session_id.as_deref().unwrap_or("—")),
        d.elapsed_ms,
        html_escape(d.model.as_deref().unwrap_or("—")),
        facts_cell,
        d.rows_returned.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string()),
        d.prompt_tokens.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string()),
        d.completion_tokens.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string()),
        d.total_tokens.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string()),
        html_escape(d.error.as_deref().unwrap_or("—")),
    ));

    // If this was a memorize, surface the extracted facts as a table.
    if let Some(facts) = extract_facts_from_response(&d.response) {
        body.push_str(&format!(r#"<h2>extracted facts <span class="pill">{} rows</span></h2>"#, facts.len()));
        body.push_str(r#"<p class="meta">Every ontological statement the LLM produced from the memorized text. Each one becomes a real <code>donto_statement</code> row in the substrate.</p>"#);
        body.push_str(r#"<table>
<thead><tr>
  <th class="num">#</th>
  <th>subject</th>
  <th>predicate</th>
  <th>object</th>
  <th>polarity</th>
  <th>modality</th>
  <th class="num">conf</th>
  <th>aperture</th>
</tr></thead><tbody>"#);
        for (i, f) in facts.iter().enumerate() {
            body.push_str(&format!(
                r#"<tr>
                  <td class="num">{}</td>
                  <td><code>{}</code></td>
                  <td><code>{}</code></td>
                  <td><code>{}</code></td>
                  <td>{}</td>
                  <td>{}</td>
                  <td class="num">{}</td>
                  <td>{}</td>
                </tr>"#,
                i + 1,
                html_escape(f.get("subject").and_then(|v| v.as_str()).unwrap_or("")),
                html_escape(f.get("predicate").and_then(|v| v.as_str()).unwrap_or("")),
                html_escape(&object_summary(f)),
                html_escape(f.get("polarity").and_then(|v| v.as_str()).unwrap_or("asserted")),
                html_escape(f.get("modality").and_then(|v| v.as_str()).unwrap_or("—")),
                f.get("confidence").and_then(|v| v.as_f64()).map(|n| format!("{:.2}", n)).unwrap_or_else(|| "—".to_string()),
                html_escape(f.get("aperture").and_then(|v| v.as_str()).unwrap_or("—")),
            ));
        }
        body.push_str("</tbody></table>");
    }

    // If recall, surface the rows.
    if let Some(rows) = d.response.get("rows").and_then(|v| v.as_array()) {
        if !rows.is_empty() {
            body.push_str(&format!(r#"<h2>recalled rows <span class="pill">{} rows</span></h2>"#, rows.len()));
            body.push_str(r#"<table><thead><tr>
                <th class="num">rank</th><th>subject</th><th>predicate</th><th>object</th>
                <th>polarity</th><th>module</th><th class="num">score</th><th>allowed</th>
            </tr></thead><tbody>"#);
            for r in rows {
                let allowed = r.get("action_allowed").and_then(|v| v.as_bool()).unwrap_or(false);
                let allowed_html = if allowed { "<span class='status-200'>✓</span>" } else { "<span class='status-500'>✗</span>" };
                body.push_str(&format!(r#"<tr>
                    <td class="num">{}</td>
                    <td><code>{}</code></td>
                    <td><code>{}</code></td>
                    <td><code>{}</code></td>
                    <td>{}</td>
                    <td>{}</td>
                    <td class="num">{}</td>
                    <td>{allowed_html}</td>
                </tr>"#,
                    r.get("rank").and_then(|v| v.as_i64()).map(|n| n.to_string()).unwrap_or_default(),
                    html_escape(r.get("subject").and_then(|v| v.as_str()).unwrap_or("")),
                    html_escape(r.get("predicate").and_then(|v| v.as_str()).unwrap_or("")),
                    html_escape(&object_summary(r)),
                    html_escape(r.get("polarity").and_then(|v| v.as_str()).unwrap_or("")),
                    html_escape(r.get("module_iri").and_then(|v| v.as_str()).unwrap_or("")),
                    r.get("score").and_then(|v| v.as_f64()).map(|n| format!("{:.4}", n)).unwrap_or_default(),
                ));
            }
            body.push_str("</tbody></table>");
        }
    }

    body.push_str(r#"<div class="grid" style="margin-top:1.5rem">"#);
    body.push_str(&format!(
        r#"<div class="card"><h3>request body</h3><pre>{}</pre></div>"#,
        html_escape(&json_pretty(&d.request))
    ));
    body.push_str(&format!(
        r#"<div class="card"><h3>response body</h3><pre>{}</pre></div>"#,
        html_escape(&json_pretty(&d.response))
    ));
    body.push_str(r#"</div>"#);

    page_chrome(&format!("Job {}", d.job_id), &body)
}

/// Pull the extracted-facts array out of a /memorize response. We
/// don't store facts as a top-level array in the response (the
/// caller gets `semantic_record_ids` + per-aperture counts), so this
/// looks at `aperture_yields[*].facts` if present, otherwise returns
/// None. (To populate facts for the table view, we attach them to
/// the logged response under a `facts` key when recording the job.)
fn extract_facts_from_response(resp: &Value) -> Option<Vec<Value>> {
    resp.get("facts").and_then(|v| v.as_array()).cloned()
}

fn object_summary(stmt: &Value) -> String {
    if let Some(o) = stmt.get("object_iri").and_then(|v| v.as_str()) {
        return o.to_string();
    }
    if let Some(o) = stmt.get("object_lit") {
        if let Some(v) = o.get("v") {
            return match v {
                Value::String(s) => format!("\"{}\"", s),
                _ => v.to_string(),
            };
        }
    }
    "—".to_string()
}

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}
