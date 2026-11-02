//! Local-only TPTP library catalogue. Nothing is downloaded or submitted remotely.
use crate::{ApiError, App, error};
use atp_core::{Problem, tptp};
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, path::PathBuf, sync::Arc};

pub struct Library {
    root: PathBuf,
    version: String,
    entries: Vec<Entry>,
    by_id: BTreeMap<String, usize>,
    domains: BTreeMap<String, usize>,
    formats: BTreeMap<String, usize>,
}
struct Entry {
    id: String,
    name: String,
    domain: String,
    format: String,
}
fn format(name: &str) -> String {
    match name.as_bytes().get(6) {
        Some(b'-') => "CNF",
        Some(b'+') => "FOF",
        Some(b'_') => "TFF",
        Some(b'^') => "THF",
        Some(b'=') => "TCF",
        _ => "Other",
    }
    .into()
}
impl Library {
    pub fn configured(project: &std::path::Path) -> Result<Option<Self>, String> {
        let configured = std::env::var("ATP_TPTP_ROOT").ok().or_else(|| {
            std::fs::read(project.join("data/library.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v["root"].as_str().map(str::to_string))
        });
        let Some(configured) = configured else {
            return Ok(None);
        };
        let root = PathBuf::from(configured)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let mut entries = Vec::new();
        let mut domains = BTreeMap::new();
        let mut formats = BTreeMap::new();
        for folder in std::fs::read_dir(root.join("Problems")).map_err(|e| e.to_string())? {
            let folder = folder.map_err(|e| e.to_string())?;
            let domain = folder.file_name().to_string_lossy().into_owned();
            if domain.len() != 3
                || !domain.bytes().all(|c| c.is_ascii_uppercase())
                || !folder.file_type().map_err(|e| e.to_string())?.is_dir()
            {
                continue;
            }
            for file in std::fs::read_dir(folder.path()).map_err(|e| e.to_string())? {
                let file = file.map_err(|e| e.to_string())?;
                let name = file.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".p") {
                    continue;
                }
                let format = format(&name);
                *domains.entry(domain.clone()).or_insert(0) += 1;
                *formats.entry(format.clone()).or_insert(0) += 1;
                entries.push(Entry {
                    id: format!("Problems/{domain}/{name}"),
                    name,
                    domain: domain.clone(),
                    format,
                });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let version = std::fs::read_to_string(root.join("Documents/ReadMe"))
            .ok()
            .and_then(|s| s.lines().next().map(str::to_owned))
            .unwrap_or_else(|| "Local TPTP library".into());
        let by_id = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        Ok(Some(Self {
            by_id,
            root,
            version,
            entries,
            domains,
            formats,
        }))
    }
    fn info(&self) -> Value {
        json!({"available":true,"root":self.root,"version":self.version,"total":self.entries.len(),"domains":self.domains,"formats":self.formats})
    }
    fn metadata(&self, e: &Entry) -> Value {
        let text = tptp::confined(&self.root, &e.id).and_then(|p| {
            let mut text = String::new();
            std::fs::File::open(p)
                .map_err(|e| e.to_string())?
                .take(32768)
                .read_to_string(&mut text)
                .map_err(|e| e.to_string())?;
            Ok(text)
        });
        match text {
            Ok(text) => {
                json!({"id":e.id,"name":e.name,"domain":e.domain,"format":e.format,"expected":tptp::header(&text,"Status").unwrap_or_else(||"Unknown".into()),"title":tptp::header(&text,"Problem"),"rating":tptp::header(&text,"Rating"),"spc":tptp::header(&text,"SPC")})
            }
            Err(message) => {
                json!({"id":e.id,"name":e.name,"domain":e.domain,"format":e.format,"expected":"Unknown","error":message})
            }
        }
    }
    pub fn reference(&self, id: &str) -> Result<Problem, String> {
        let entry = self
            .by_id
            .get(id)
            .and_then(|&i| self.entries.get(i))
            .ok_or("Unknown library problem")?;
        Ok(Problem {
            name: entry.name.clone(),
            content: String::new(),
            expected: "Unknown".into(),
            source: Some(format!("{} / {}", self.version, id)),
            library_id: Some(id.into()),
        })
    }
    /// Read just the next input when the worker is ready, never the whole suite.
    pub fn load(&self, id: &str) -> Result<Problem, String> {
        let mut problem = self.reference(id)?;
        let path = tptp::confined(&self.root, id)?;
        let mut header = String::new();
        std::fs::File::open(path)
            .map_err(|e| e.to_string())?
            .take(32768)
            .read_to_string(&mut header)
            .map_err(|e| e.to_string())?;
        let status = tptp::header(&header, "Status").unwrap_or_else(|| "Unknown".into());
        problem.expected = if atp_core::definitive(&status) {
            status
        } else {
            "Unknown".into()
        };
        problem.content = tptp::expand(&self.root, id, 64 * 1024 * 1024)?;
        Ok(problem)
    }
    fn import(&self, ids: Vec<String>) -> Result<Vec<Problem>, String> {
        if ids.is_empty() {
            return Err("Select at least one library problem".into());
        }
        let mut seen = std::collections::HashSet::new();
        ids.into_iter()
            .filter(|id| seen.insert(id.clone()))
            .map(|id| self.reference(&id))
            .collect()
    }
}
#[derive(Default, Deserialize)]
pub struct Search {
    #[serde(default)]
    q: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    offset: usize,
}
#[derive(Deserialize)]
pub struct Import {
    ids: Vec<String>,
}
fn get(app: &App) -> Result<Arc<Library>, ApiError> {
    app.library.clone().ok_or_else(|| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            app.library_error
                .as_deref()
                .unwrap_or("Set ATP_TPTP_ROOT or data/library.json to configure a local library"),
        )
    })
}
pub async fn info(State(app): State<App>) -> Json<Value> {
    Json(match &app.library {
        Some(l) => l.info(),
        None => json!({"available":false,"error":app.library_error}),
    })
}
/// Lightweight directory index; metadata is still loaded one page at a time.
pub async fn tree(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let library = get(&app)?;
    let mut directories: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for entry in &library.entries {
        directories
            .entry(&entry.domain)
            .or_default()
            .push(json!({"id":entry.id,"name":entry.name,"format":entry.format}));
    }
    Ok(Json(
        json!({"directories":directories.into_iter().map(|(name,problems)|json!({"id":format!("Problems/{name}"),"name":name,"problems":problems})).collect::<Vec<_>>()}),
    ))
}
pub async fn search(
    State(app): State<App>,
    Query(query): Query<Search>,
) -> Result<Json<Value>, ApiError> {
    let library = get(&app)?;
    tokio::task::spawn_blocking(move || {
        let q = query.q.trim().to_ascii_lowercase();
        let filtered: Vec<_> = library
            .entries
            .iter()
            .filter(|e| {
                (query.domain.is_empty() || e.domain == query.domain)
                    && (query.format.is_empty() || e.format == query.format)
                    && (q.is_empty() || e.name.to_ascii_lowercase().contains(&q))
            })
            .collect();
        let total = filtered.len();
        let entries: Vec<_> = filtered
            .into_iter()
            .skip(query.offset)
            .take(20)
            .map(|e| library.metadata(e))
            .collect();
        Json(json!({"total":total,"offset":query.offset,"limit":20,"entries":entries}))
    })
    .await
    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))
}
pub async fn import(
    State(app): State<App>,
    Json(request): Json<Import>,
) -> Result<Json<Value>, ApiError> {
    let library = get(&app)?;
    tokio::task::spawn_blocking(move || library.import(request.ids))
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .map(|p| Json(json!({"problems":p})))
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))
}

pub async fn preview(
    State(app): State<App>,
    Json(request): Json<Preview>,
) -> Result<Json<Problem>, ApiError> {
    let library = get(&app)?;
    tokio::task::spawn_blocking(move || library.load(&request.id))
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .map(Json)
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))
}
#[derive(Deserialize)]
pub struct Preview {
    id: String,
}
