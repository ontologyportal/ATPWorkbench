//! Bounded input loading and append-only results for jobs of any problem count.
use super::*;
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path as FsPath,
};

/// Transient worker state. Never infer the active problem from completed rows.
#[derive(Clone)]
pub struct CurrentExecution {
    problem: String,
    problem_index: usize,
    prover: Option<String>,
    prover_index: Option<usize>,
    execution_index: Option<usize>,
    phase: &'static str,
    started: std::time::Instant,
}
fn current(app: &App, id: &str, execution: CurrentExecution) {
    if let Ok(mut batches) = app.batches.lock()
        && let Some(batch) = batches.get_mut(id)
    {
        batch.current_execution = Some(execution);
    }
}
fn current_value(batch: &Batch) -> Value {
    batch.current_execution.as_ref().map(|c|json!({"problem":c.problem,"problem_index":c.problem_index,"prover":c.prover,"prover_index":c.prover_index,"execution_index":c.execution_index,"phase":c.phase,"elapsed_seconds":c.started.elapsed().as_secs_f64()})).unwrap_or(Value::Null)
}
pub fn header(root: &FsPath, batch: &Batch) -> Result<(), String> {
    let folder = root.join("data/runs").join(&batch.id);
    let value = json!({"storage_version":1,"id":batch.id,"name":batch.name,"created":batch.created,"state":batch.state,"seconds":batch.seconds,"problems":batch.problems,"provers":batch.provers,"manifest":batch.manifest,"results":[],"total":batch.total,"error":batch.error});
    let tmp = folder.join("batch.tmp");
    let mut file = File::create(&tmp).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, &value).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(tmp, folder.join("batch.json")).map_err(|e| e.to_string())
}
pub fn append<T: Serialize>(path: &FsPath, value: &T) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_data())
        .map_err(|e| e.to_string())
}
/// Ignore only an incomplete final record left by an interrupted write.
pub fn records<T: serde::de::DeserializeOwned>(path: &FsPath) -> Result<Vec<T>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut reader = BufReader::new(File::open(path).map_err(|e| e.to_string())?);
    let mut rows = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?
            == 0
        {
            break;
        }
        match serde_json::from_slice(&line) {
            Ok(row) => rows.push(row),
            Err(_) if !line.ends_with(b"\n") => break,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Ok(rows)
}
pub fn restore(folder: &FsPath, batch: &mut Batch) -> Result<(), String> {
    batch.current_execution = None;
    if batch.storage_version == 1 {
        batch.results = records(&folder.join("results.jsonl"))?;
        for input in records::<Value>(&folder.join("inputs.jsonl"))? {
            if let Some(index) = input["index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                && let Some(problem) = batch.problems.get_mut(index)
                && let Some(expected) = input["expected"].as_str()
            {
                problem.expected = expected.into();
            }
        }
    }
    Ok(())
}
fn save_input(folder: &FsPath, index: usize, problem: &Problem) -> Result<(), String> {
    let sha256 = format!("{:x}", Sha256::digest(problem.content.as_bytes()));
    let path = format!("inputs/{index}.json");
    let mut file = File::create(folder.join(&path)).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, problem).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    append(
        &folder.join("inputs.jsonl"),
        &json!({"index":index,"name":problem.name,"source":problem.source,"library_id":problem.library_id,"expected":problem.expected,"sha256":sha256,"path":path}),
    )
}
fn commit(app: &App, batch: &Batch, folder: &FsPath, row: ResultRow) -> Result<(), String> {
    append(&folder.join("results.jsonl"), &row)?;
    app.batches
        .lock()
        .map_err(|e| e.to_string())?
        .get_mut(&batch.id)
        .ok_or("Job missing")?
        .results
        .push(row);
    Ok(())
}
fn input_error(
    prover: &atp_runner::Prover,
    problem: &Problem,
    message: &str,
    index: usize,
    folder: &FsPath,
) -> ResultRow {
    let output_file = format!("input-error-{index}.log");
    let _ = std::fs::write(folder.join(&output_file), message);
    ResultRow {
        prover: prover.id.clone(),
        problem: problem.name.clone(),
        expected: problem.expected.clone(),
        state: "error".into(),
        status: "InputError".into(),
        wall_seconds: 0.0,
        cpu_seconds: None,
        peak_rss_kb: None,
        exit_code: None,
        output_present: false,
        output_file,
        error: Some(message.into()),
    }
}
pub async fn worker(app: App, mut batch: Batch, selected: Vec<atp_runner::Prover>) {
    let folder = app.root.join("data/runs").join(&batch.id);
    let mut result_index = 0;
    'job: for (problem_index, reference) in batch.problems.iter().enumerate() {
        if app.cancel.load(Ordering::Relaxed) {
            break;
        }
        current(
            &app,
            &batch.id,
            CurrentExecution {
                problem: reference.name.clone(),
                problem_index: problem_index + 1,
                prover: None,
                prover_index: None,
                execution_index: None,
                phase: "preparing",
                started: std::time::Instant::now(),
            },
        );
        let reference = reference.clone();
        let library = app.library.clone();
        let input_folder = folder.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            let loaded = match &reference.library_id {
                Some(id) => library
                    .ok_or_else(|| "Library unavailable".to_string())
                    .and_then(|library| library.load(id)),
                None => Ok(reference),
            };
            if let Ok(problem) = &loaded {
                save_input(&input_folder, problem_index, problem)?;
            }
            Ok::<Result<Problem, String>, String>(loaded)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
        let loaded = match prepared {
            Ok(problem) => problem,
            Err(message) => {
                batch.error = Some(message);
                break 'job;
            }
        };
        for (prover_index, prover) in selected.iter().enumerate() {
            if app.cancel.load(Ordering::Relaxed) {
                break 'job;
            }
            current(
                &app,
                &batch.id,
                CurrentExecution {
                    problem: batch.problems[problem_index].name.clone(),
                    problem_index: problem_index + 1,
                    prover: Some(prover.id.clone()),
                    prover_index: Some(prover_index + 1),
                    execution_index: Some(result_index + 1),
                    phase: if loaded.is_ok() {
                        "running"
                    } else {
                        "input_error"
                    },
                    started: std::time::Instant::now(),
                },
            );
            let row = match &loaded {
                Ok(problem) => {
                    execute(
                        prover,
                        problem,
                        batch.seconds,
                        &folder,
                        result_index,
                        app.cancel.clone(),
                    )
                    .await
                }
                Err(message) => input_error(
                    prover,
                    &batch.problems[problem_index],
                    message,
                    result_index,
                    &folder,
                ),
            };
            if let Ok(mut batches) = app.batches.lock()
                && let Some(saved) = batches.get_mut(&batch.id)
                && let Some(current) = saved.current_execution.as_mut()
            {
                current.phase = "saving";
            }
            if let Err(e) = commit(&app, &batch, &folder, row) {
                batch.error = Some(e);
                break 'job;
            }
            result_index += 1;
        }
    }
    batch.state = if batch.error.is_some() {
        "failed"
    } else if app.cancel.load(Ordering::Relaxed) {
        "cancelled"
    } else {
        "finished"
    }
    .into();
    if let Err(e) = header(&app.root, &batch) {
        batch.error = Some(e);
        batch.state = "failed".into();
    }
    if let Ok(mut batches) = app.batches.lock()
        && let Some(saved) = batches.get_mut(&batch.id)
    {
        saved.current_execution = None;
        saved.state = batch.state;
        saved.error = batch.error;
    }
    app.active.store(false, Ordering::SeqCst);
}
#[derive(Default, Deserialize)]
pub struct View {
    #[serde(default)]
    compact: bool,
    offset: Option<usize>,
}
fn profiles(batch: &Batch) -> Vec<Value> {
    batch
        .provers
        .iter()
        .map(|id| {
            let mut times: Vec<_> = batch
                .results
                .iter()
                .filter(|r| &r.prover == id && r.matched())
                .filter_map(|r| r.cpu_seconds)
                .collect();
            times.sort_by(f64::total_cmp);
            let stride = times.len().div_ceil(200).max(1);
            let mut total = 0.0;
            let mut points = vec![json!({"x":0,"y":0})];
            for (i, time) in times.iter().enumerate() {
                total += time;
                if (i + 1) % stride == 0 || i + 1 == times.len() {
                    points.push(json!({"x":total,"y":i+1}));
                }
            }
            json!({"id":id,"points":points})
        })
        .collect()
}
pub async fn view(
    State(app): State<App>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<View>,
) -> Result<Json<Value>, ApiError> {
    Uuid::parse_str(&id).map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid job ID"))?;
    let batches = app
        .batches
        .lock()
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "State unavailable"))?;
    let b = batches
        .get(&id)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Job not found"))?;
    let last_page = b.results.len().saturating_sub(1) / 100 * 100;
    let offset = query.offset.unwrap_or(last_page).min(last_page);
    let visible = &b.results[offset..(offset + 100).min(b.results.len())];
    Ok(Json(
        json!({"completed":b.results.len(),"problem_count":b.problems.len(),"cpu_seconds":b.results.iter().filter_map(|r|r.cpu_seconds).sum::<f64>(),"result_offset":offset,"result_limit":100,"profile":profiles(b),"standings":standings(&b.results,&b.provers),"batch":{"current_execution":current_value(b),"id":b.id,"name":b.name,"created":b.created,"state":b.state,"seconds":b.seconds,"provers":b.provers,"manifest":if query.compact{Value::Null}else{b.manifest.clone()},"problems":if query.compact{json!([])}else{json!(b.problems)},"results":visible,"total":b.total,"error":b.error}}),
    ))
}
pub async fn export_json(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let b = find(&app, &id)?;
    let inputs = records::<Value>(&app.root.join("data/runs").join(&id).join("inputs.jsonl"))
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let value =
        json!({"standings":standings(&b.results,&b.provers),"batch":b,"input_snapshots":inputs});
    Ok((
        [(
            header::CONTENT_DISPOSITION,
            "attachment; filename=atp-job.json",
        )],
        Json(value),
    ))
}
pub async fn input(
    State(app): State<App>,
    Path((id, index)): Path<(String, usize)>,
) -> Result<Json<Problem>, ApiError> {
    Uuid::parse_str(&id).map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid job ID"))?;
    let bytes = tokio::fs::read(
        app.root
            .join("data/runs")
            .join(id)
            .join(format!("inputs/{index}.json")),
    )
    .await
    .map_err(|e| error(StatusCode::NOT_FOUND, e))?;
    serde_json::from_slice(&bytes)
        .map(Json)
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!("atp-job-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("Problems/TST")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(
            root.join("data/library.json"),
            serde_json::to_vec(&json!({"root":root})).unwrap(),
        )
        .unwrap();
        root
    }
    #[test]
    fn journal_recovers_complete_records_and_detects_corruption() {
        let root = fixture();
        let path = root.join("records.jsonl");
        append(&path, &json!({"n":1})).unwrap();
        append(&path, &json!({"n":2})).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"n\":")
            .unwrap();
        let rows = records::<Value>(&path).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["n"], 2);
        std::fs::write(&path, b"{bad}\n").unwrap();
        assert!(records::<Value>(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn one_job_streams_over_one_hundred_inputs_and_continues_after_input_error() {
        let root = fixture();
        for index in 0..121 {
            std::fs::write(
                root.join(format!("Problems/TST/TST{index:03}+1.p")),
                if index == 0 {
                    "include('missing.ax')."
                } else {
                    "% Status : Theorem\nfof(goal,conjecture,p=>p)."
                },
            )
            .unwrap();
        }
        let library = Arc::new(library::Library::configured(&root).unwrap().unwrap());
        let problems: Vec<_> = (0..121)
            .map(|i| {
                library
                    .reference(&format!("Problems/TST/TST{i:03}+1.p"))
                    .unwrap()
            })
            .collect();
        assert!(problems.iter().all(|p| p.content.is_empty()));
        let batch = Batch {
            storage_version: 1,
            current_execution: None,
            id: Uuid::new_v4().to_string(),
            name: "Large fixture".into(),
            created: 0,
            state: "running".into(),
            seconds: 1,
            problems,
            provers: vec!["fixture".into()],
            manifest: json!({}),
            results: vec![],
            total: 121,
            error: None,
        };
        let folder = root.join("data/runs").join(&batch.id);
        std::fs::create_dir_all(folder.join("inputs")).unwrap();
        header(&root, &batch).unwrap();
        let app = App {
            provers: Arc::new(atp_runner::local_provers()),
            root: root.clone(),
            library: Some(library),
            library_error: None,
            batches: Arc::new(Mutex::new(BTreeMap::from([(
                batch.id.clone(),
                batch.clone(),
            )]))),
            active: Arc::new(AtomicBool::new(true)),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let prover = atp_runner::Prover {
            id: "fixture".into(),
            name: "Fixture".into(),
            version: "test".into(),
            executable: "/usr/bin/printf".into(),
            env: vec![],
            unavailable_reason: None,
            args: vec!["%% SZS status Theorem\n".into()],
        };
        worker(app.clone(), batch.clone(), vec![prover]).await;
        let saved = app.batches.lock().unwrap().get(&batch.id).unwrap().clone();
        assert_eq!(saved.state, "finished");
        assert!(saved.current_execution.is_none());
        assert_eq!(saved.results.len(), 121);
        assert_eq!(saved.results[0].status, "InputError");
        assert_eq!(standings(&saved.results, &saved.provers)[0].matches, 120);
        assert!(!app.active.load(Ordering::SeqCst));
        let mut disk: Batch =
            serde_json::from_slice(&std::fs::read(folder.join("batch.json")).unwrap()).unwrap();
        assert!(disk.results.is_empty());
        assert!(disk.problems.iter().all(|p| p.content.is_empty()));
        restore(&folder, &mut disk).unwrap();
        assert_eq!(disk.results.len(), 121);
        assert_eq!(disk.problems[120].expected, "Theorem");
        let inputs = records::<Value>(&folder.join("inputs.jsonl")).unwrap();
        assert_eq!(inputs.len(), 120);
        let input: Problem =
            serde_json::from_slice(&std::fs::read(folder.join("inputs/120.json")).unwrap())
                .unwrap();
        assert_eq!(
            inputs[119]["sha256"],
            format!("{:x}", Sha256::digest(input.content.as_bytes()))
        );
        let view_page = view(
            State(app.clone()),
            Path(batch.id.clone()),
            axum::extract::Query(View {
                compact: true,
                offset: Some(100),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(view_page["completed"], 121);
        assert_eq!(view_page["problem_count"], 121);
        assert_eq!(view_page["result_offset"], 100);
        assert_eq!(view_page["batch"]["results"].as_array().unwrap().len(), 21);
        assert!(
            view_page["batch"]["problems"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(view_page["standings"][0]["matches"], 120);
        let first_page = view(
            State(app.clone()),
            Path(batch.id.clone()),
            axum::extract::Query(View {
                compact: false,
                offset: Some(0),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(
            first_page["batch"]["results"].as_array().unwrap().len(),
            100
        );
        assert_eq!(
            first_page["batch"]["problems"].as_array().unwrap().len(),
            121
        );
        let mut cancelled = batch.clone();
        cancelled.id = Uuid::new_v4().to_string();
        std::fs::create_dir_all(root.join("data/runs").join(&cancelled.id).join("inputs")).unwrap();
        app.batches
            .lock()
            .unwrap()
            .insert(cancelled.id.clone(), cancelled.clone());
        app.cancel.store(true, Ordering::SeqCst);
        app.active.store(true, Ordering::SeqCst);
        worker(app.clone(), cancelled.clone(), vec![]).await;
        assert_eq!(
            app.batches.lock().unwrap()[&cancelled.id].state,
            "cancelled"
        );
        assert!(
            app.batches.lock().unwrap()[&cancelled.id]
                .results
                .is_empty()
        );
        let mut unwritable = batch.clone();
        unwritable.id = Uuid::new_v4().to_string();
        let broken = root.join("data/runs").join(&unwritable.id);
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("inputs"), "this is a file, not a directory").unwrap();
        app.batches
            .lock()
            .unwrap()
            .insert(unwritable.id.clone(), unwritable.clone());
        app.cancel.store(false, Ordering::SeqCst);
        app.active.store(true, Ordering::SeqCst);
        worker(app.clone(), unwritable.clone(), vec![]).await;
        assert_eq!(app.batches.lock().unwrap()[&unwritable.id].state, "failed");
        assert!(app.batches.lock().unwrap()[&unwritable.id].error.is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    #[tokio::test]
    async fn reports_real_worker_position_and_clears_it_after_cancellation() {
        let root = std::env::temp_dir().join(format!("atp-live-test-{}", Uuid::new_v4()));
        let batch = Batch {
            storage_version: 1,
            current_execution: None,
            id: Uuid::new_v4().to_string(),
            name: "Live fixture".into(),
            created: 0,
            state: "running".into(),
            seconds: 3,
            problems: vec![Problem {
                name: "current.p".into(),
                content: "fof(goal,conjecture,p=>p).".into(),
                expected: "Theorem".into(),
                source: None,
                library_id: None,
            }],
            provers: vec!["fixture".into()],
            manifest: json!({}),
            results: vec![],
            total: 1,
            error: None,
        };
        let folder = root.join("data/runs").join(&batch.id);
        std::fs::create_dir_all(folder.join("inputs")).unwrap();
        header(&root, &batch).unwrap();
        let app = App {
            provers: Arc::new(atp_runner::local_provers()),
            root: root.clone(),
            library: None,
            library_error: None,
            batches: Arc::new(Mutex::new(BTreeMap::from([(
                batch.id.clone(),
                batch.clone(),
            )]))),
            active: Arc::new(AtomicBool::new(true)),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let prover = atp_runner::Prover {
            id: "fixture".into(),
            name: "Fixture".into(),
            version: "test".into(),
            executable: "/bin/sh".into(),
            env: vec![],
            unavailable_reason: None,
            args: vec!["-c".into(), "sleep 10".into()],
        };
        let worker = tokio::spawn(worker(app.clone(), batch.clone(), vec![prover]));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let running = app.batches.lock().unwrap()[&batch.id]
                    .current_execution
                    .as_ref()
                    .is_some_and(|c| c.phase == "running");
                if running {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let live = view(
            State(app.clone()),
            Path(batch.id.clone()),
            axum::extract::Query(View {
                compact: true,
                offset: None,
            }),
        )
        .await
        .unwrap()
        .0;
        let current = &live["batch"]["current_execution"];
        assert_eq!(current["problem"], "current.p");
        assert_eq!(current["problem_index"], 1);
        assert_eq!(current["prover"], "fixture");
        assert_eq!(current["prover_index"], 1);
        assert_eq!(current["execution_index"], 1);
        assert_eq!(current["phase"], "running");
        assert!(current["elapsed_seconds"].as_f64().unwrap() >= 0.0);
        app.cancel.store(true, Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        let saved = app.batches.lock().unwrap()[&batch.id].clone();
        assert_eq!(saved.state, "cancelled");
        assert!(saved.current_execution.is_none());
        let disk: Batch =
            serde_json::from_slice(&std::fs::read(folder.join("batch.json")).unwrap()).unwrap();
        assert!(disk.current_execution.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
