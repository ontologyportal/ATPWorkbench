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
        Ok(Some(Self {
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
    fn import(&self, ids: Vec<String>) -> Result<Vec<Problem>, String> {
        if ids.is_empty() || ids.len() > 100 {
            return Err("Select 1-100 library problems".into());
        }
        let mut unique = std::collections::HashSet::new();
        let mut total = 0;
        let mut problems = Vec::new();
        for id in ids {
            if !unique.insert(id.clone()) {
                continue;
            }
            let entry = self
                .entries
                .iter()
                .find(|e| e.id == id)
                .ok_or("Unknown library problem")?;
            let path = tptp::confined(&self.root, &id)?;
            let original = std::fs::File::open(path).map_err(|e| e.to_string())?;
            let mut header = String::new();
            original
                .take(32768)
                .read_to_string(&mut header)
                .map_err(|e| e.to_string())?;
            let expected = tptp::header(&header, "Status").unwrap_or_else(|| "Unknown".into());
            let expected = if [
                "Theorem",
                "Unsatisfiable",
                "Satisfiable",
                "CounterSatisfiable",
            ]
            .contains(&expected.as_str())
            {
                expected
            } else {
                "Unknown".into()
            };
            let content = tptp::expand(&self.root, &id, 1024 * 1024)
                .map_err(|e| format!("{}: {e}", entry.name))?;
            total += content.len();
            if total > 8 * 1024 * 1024 {
                return Err("Import exceeds the 8 MB batch limit; select fewer problems".into());
            }
            problems.push(Problem {
                name: entry.name.clone(),
                content,
                expected,
                source: Some(format!("{} / {}", self.version, id)),
            });
        }
        Ok(problems)
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
