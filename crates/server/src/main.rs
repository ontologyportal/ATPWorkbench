mod jobs;
mod library;
use atp_core::{Problem, ResultRow, standings};
use atp_runner::{Prover, discover_provers, execute};
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;
type ApiError = (StatusCode, Json<Value>);
fn error(code: StatusCode, message: impl ToString) -> ApiError {
    (code, Json(json!({"error":message.to_string()})))
}
#[derive(Clone)]
struct App {
    provers: Arc<Vec<Prover>>,
    root: PathBuf,
    library: Option<Arc<library::Library>>,
    library_error: Option<String>,
    batches: Arc<Mutex<BTreeMap<String, Batch>>>,
    active: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Batch {
    #[serde(default)]
    storage_version: usize,
    #[serde(skip)]
    current_execution: Option<jobs::CurrentExecution>,
    id: String,
    name: String,
    created: u64,
    state: String,
    seconds: u64,
    problems: Vec<Problem>,
    provers: Vec<String>,
    manifest: Value,
    results: Vec<ResultRow>,
    total: usize,
    error: Option<String>,
}
#[derive(Deserialize)]
struct Submission {
    name: String,
    seconds: u64,
    problems: Vec<Problem>,
    provers: Vec<String>,
}
fn find(app: &App, id: &str) -> Result<Batch, ApiError> {
    Uuid::parse_str(id).map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid batch ID"))?;
    app.batches
        .lock()
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "State unavailable"))?
        .get(id)
        .cloned()
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Batch not found"))
}
async fn loopback(request: Request, next: Next) -> Response {
    let h = request.headers();
    let port = std::env::var("ATP_PORT").unwrap_or_else(|_| "7878".into());
    let allowed = [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        "localhost:5173".into(),
        "127.0.0.1:5173".into(),
    ];
    let host = h
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let origin_ok = h.get(header::ORIGIN).is_none_or(|v| {
        v.to_str()
            .is_ok_and(|o| allowed.iter().any(|host| o == format!("http://{host}")))
    });
    if !allowed.iter().any(|allowed| allowed == host) || !origin_ok {
        return error(StatusCode::FORBIDDEN, "Local requests only").into_response();
    }
    next.run(request).await
}
async fn provers(State(app): State<App>) -> Json<Value> {
    Json(json!(app.provers.iter().map(|p|json!({"id":p.id,"name":p.name,"version":p.version,"installed":p.is_available(),"availability_error":p.unavailable_reason,"executable":p.executable,"exact_catalogue_verified":false})).collect::<Vec<_>>() ))
}
async fn list(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let batches = app
        .batches
        .lock()
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "State unavailable"))?;
    let mut values:Vec<_>=batches.values().map(|b|json!({"id":b.id,"name":b.name,"created":b.created,"state":b.state,"total":b.total,"completed":b.results.len()})).collect();
    values.sort_by_key(|v| std::cmp::Reverse(v["created"].as_u64()));
    Ok(Json(json!(values)))
}
fn validate(s: &Submission, known: &[Prover]) -> Result<(), String> {
    if s.name.trim().is_empty() || s.name.len() > 120 {
        return Err("Suite name must be 1-120 characters".into());
    }
    if !(1..=300).contains(&s.seconds) {
        return Err("Time limit must be 1-300 seconds".into());
    }
    if s.problems.is_empty() {
        return Err("Select at least one problem".into());
    }
    let mut names = HashSet::new();
    for p in &s.problems {
        if p.name.trim().is_empty() || p.name.len() > 160 || !names.insert(&p.name) {
            return Err("Problem names must be nonempty and unique".into());
        }
        if p.library_id.is_some() {
            continue;
        }
        if p.content.trim().is_empty() || p.content.len() > 1024 * 1024 {
            return Err("Each problem must contain 1 byte to 1 MB of input".into());
        }
        if atp_core::tptp::contains_include(&p.content)? {
            return Err("Upload self-contained input, or import through the TPTP library to resolve includes".into());
        }
        if ![
            "Theorem",
            "Unsatisfiable",
            "Satisfiable",
            "CounterSatisfiable",
            "Unknown",
        ]
        .contains(&p.expected.as_str())
        {
            return Err("Invalid expected SZS status".into());
        }
    }
    let mut ids = HashSet::new();
    if s.provers.is_empty()
        || s.provers
            .iter()
            .any(|id| !ids.insert(id) || !known.iter().any(|p| &p.id == id && p.is_available()))
    {
        return Err("Select installed provers with no duplicates".into());
    }
    Ok(())
}
async fn submit(
    State(app): State<App>,
    Json(mut s): Json<Submission>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    for problem in &mut s.problems {
        if let Some(id) = &problem.library_id {
            *problem = app
                .library
                .as_ref()
                .ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "Library unavailable"))?
                .reference(id)
                .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
        }
    }
    validate(&s, &app.provers).map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    if app
        .active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(error(StatusCode::CONFLICT, "A suite is already running"));
    }
    app.cancel.store(false, Ordering::SeqCst);
    let id = Uuid::new_v4().to_string();
    let folder = app.root.join("data/runs").join(&id);
    if let Err(e) = std::fs::create_dir_all(&folder) {
        app.active.store(false, Ordering::SeqCst);
        return Err(error(StatusCode::INTERNAL_SERVER_ERROR, e));
    }
    let selected: Vec<_> = app
        .provers
        .iter()
        .cloned()
        .filter(|p| s.provers.contains(&p.id))
        .collect();
    let manifest = json!({"provers":selected.iter().map(|p|json!({"id":p.id,"version":p.version,"executable":p.executable,"arguments":p.args,"environment":p.env})).collect::<Vec<_>>(),"inputs":s.problems.iter().filter(|p|p.library_id.is_none()).map(|p|json!({"name":p.name,"source":p.source,"sha256":format!("{:x}",Sha256::digest(p.content.as_bytes()))})).collect::<Vec<_>>(),"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"timing":"GNU time CPU user+system, process wall, peak RSS","proof_validation":"not implemented","ranking":"expected-status matches, then average matching CPU, then wall","execution":"sequential local subprocesses, one input loaded at a time","input_batch_size":1,"input_snapshots":"inputs.jsonl","library_input_limit_bytes":64*1024*1024});
    let batch = Batch {
        storage_version: 1,
        current_execution: None,
        id: id.clone(),
        name: s.name,
        created: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        state: "running".into(),
        seconds: s.seconds,
        total: s.problems.len() * s.provers.len(),
        problems: s.problems,
        provers: s.provers,
        manifest,
        results: vec![],
        error: None,
    };
    if let Err(e) = std::fs::create_dir_all(folder.join("inputs"))
        .map_err(|e| e.to_string())
        .and_then(|_| jobs::header(&app.root, &batch))
    {
        app.active.store(false, Ordering::SeqCst);
        return Err(error(StatusCode::INTERNAL_SERVER_ERROR, e));
    }
    app.batches
        .lock()
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "State unavailable"))?
        .insert(id.clone(), batch.clone());
    tokio::spawn(jobs::worker(app, batch, selected));
    Ok((StatusCode::ACCEPTED, Json(json!({"id":id}))))
}
async fn cancel(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    if find(&app, &id)?.state != "running" {
        return Err(error(StatusCode::CONFLICT, "Batch is not running"));
    }
    app.cancel.store(true, Ordering::SeqCst);
    Ok(Json(json!({"state":"cancelling"})))
}
async fn output(
    State(app): State<App>,
    Path((id, index)): Path<(String, usize)>,
) -> Result<impl IntoResponse, ApiError> {
    let b = find(&app, &id)?;
    let r = b
        .results
        .get(index)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Result not found"))?;
    let bytes = tokio::fs::read(app.root.join("data/runs").join(&id).join(&r.output_file))
        .await
        .map_err(|e| error(StatusCode::NOT_FOUND, e))?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], bytes))
}
fn csv_cell(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
async fn export(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let b = find(&app, &id)?;
    let mut csv = String::from(
        "prover,problem,expected,status,expected_status_match,wall_seconds,cpu_seconds,peak_rss_kb,exit_code,output_present,error\n",
    );
    for r in &b.results {
        csv.push_str(&format!(
            "{},{},{},{},{},{:.6},{},{},{},{},{}\n",
            csv_cell(&r.prover),
            csv_cell(&r.problem),
            csv_cell(&r.expected),
            csv_cell(&r.status),
            r.matched(),
            r.wall_seconds,
            r.cpu_seconds.map(|x| x.to_string()).unwrap_or_default(),
            r.peak_rss_kb.map(|x| x.to_string()).unwrap_or_default(),
            r.exit_code.map(|x| x.to_string()).unwrap_or_default(),
            r.output_present,
            csv_cell(r.error.as_deref().unwrap_or(""))
        ));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=atp-results.csv",
            ),
        ],
        csv,
    ))
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let storage_root = std::env::var_os("ATP_STORAGE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.clone());
    let runs = storage_root.join("data/runs");
    std::fs::create_dir_all(&runs)?;
    let mut batches = BTreeMap::new();
    for entry in std::fs::read_dir(&runs)? {
        let path = entry?.path().join("batch.json");
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(mut b) = serde_json::from_slice::<Batch>(&bytes)
        {
            if let Err(e) = jobs::restore(path.parent().ok_or("Missing run directory")?, &mut b) {
                eprintln!("Cannot restore job {}: {e}", b.id);
                b.state = "failed".into();
                b.error = Some(e);
            }
            if b.state == "running" {
                b.state = "interrupted".into();
                std::fs::write(path, serde_json::to_vec_pretty(&b)?)?;
            }
            batches.insert(b.id.clone(), b);
        }
    }
    let (library, library_error) = match library::Library::configured(&root) {
        Ok(l) => (l.map(Arc::new), None),
        Err(e) => {
            eprintln!("TPTP library unavailable: {e}");
            (None, Some(e))
        }
    };
    let configured_provers = discover_provers().await;
    let app = App {
        provers: Arc::new(configured_provers),
        library,
        library_error,
        root: storage_root,
        batches: Arc::new(Mutex::new(batches)),
        active: Arc::new(AtomicBool::new(false)),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let shutdown_app = app.clone();
    let web = root.join("packages/web/dist");
    let router = Router::new()
        .route(
            "/api/health",
            get(|| async { Json(json!({"status":"ok","execution":"local"})) }),
        )
        .route("/api/library", get(library::info))
        .route("/api/library/tree", get(library::tree))
        .route("/api/library/problems", get(library::search))
        .route("/api/library/import", post(library::import))
        .route("/api/library/preview", post(library::preview))
        .route("/api/provers", get(provers))
        .route("/api/batches", get(list).post(submit))
        .route("/api/batches/{id}", get(jobs::view))
        .route("/api/batches/{id}/export.json", get(jobs::export_json))
        .route("/api/batches/{id}/input/{index}", get(jobs::input))
        .route("/api/batches/{id}/cancel", post(cancel))
        .route("/api/batches/{id}/output/{index}", get(output))
        .route("/api/batches/{id}/export.csv", get(export))
        .layer(axum::extract::DefaultBodyLimit::max(12 * 1024 * 1024))
        .fallback_service(
            ServeDir::new(&web).not_found_service(ServeFile::new(web.join("index.html"))),
        )
        .layer(middleware::from_fn(loopback))
        .with_state(app);
    let port = std::env::var("ATP_PORT")
        .unwrap_or_else(|_| "7878".into())
        .parse::<u16>()?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    println!("ATPWorkbench: http://localhost:{port} (Vite development: http://localhost:5173)");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown_app.cancel.store(true, Ordering::SeqCst);
            while shutdown_app.active.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_bad_limits_and_includes() {
        let mut s = Submission {
            name: "Suite".into(),
            seconds: 0,
            provers: vec!["e".into()],
            problems: vec![Problem {
                name: "a".into(),
                content: "fof(a,axiom,p).".into(),
                expected: "Theorem".into(),
                source: None,
                library_id: None,
            }],
        };
        assert!(validate(&s, &atp_runner::local_provers()).is_err());
        s.seconds = 5;
        s.problems[0].content = "include('secret').".into();
        assert!(validate(&s, &atp_runner::local_provers()).is_err());
    }
}
