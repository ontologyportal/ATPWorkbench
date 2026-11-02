use atp_core::{Problem, ResultRow, szs_status};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tokio::{
    process::Command,
    time::{Duration, sleep},
};

struct ProcessGroup(i32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = killpg(Pid::from_raw(self.0), Signal::SIGKILL);
    }
}

#[derive(Clone, Debug)]
pub struct Prover {
    pub id: String,
    pub name: String,
    pub version: String,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub unavailable_reason: Option<String>,
}

pub fn executable_available(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
impl Prover {
    pub fn is_available(&self) -> bool {
        self.unavailable_reason.is_none() && executable_available(&self.executable)
    }
}
fn on_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|path| executable_available(path))
        .and_then(|path| path.canonicalize().ok())
}
fn sumo_path(home: &Path) -> PathBuf {
    on_path("sumo", &std::env::var_os("PATH").unwrap_or_default())
        .unwrap_or_else(|| home.join(".sigmakee/bin/sumo"))
}
/// Bounded CLI probes keep missing/incompatible executables out of the run selector.
pub async fn discover_provers() -> Vec<Prover> {
    let mut provers = local_provers();
    for prover in provers.iter_mut().filter(|p| p.id == "supr") {
        if !executable_available(&prover.executable) {
            prover.unavailable_reason=Some("Install Sigma's sumo CLI on PATH (or ~/.sigmakee/bin/sumo), then restart the backend".into());
            continue;
        }
        let probe = async {
            let version = Command::new(&prover.executable)
                .arg("--version")
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| e.to_string())?;
            if !version.status.success() {
                return Err("sumo --version failed".to_string());
            }
            let version = String::from_utf8_lossy(&version.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(300)
                .collect::<String>();
            let help = Command::new(&prover.executable)
                .args(["casc", "--help"])
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| e.to_string())?;
            if !help.status.success() {
                return Err("sumo casc --help failed".to_string());
            }
            let help = String::from_utf8_lossy(&help.stdout);
            if !version.starts_with("sumo ")
                || !help.contains("sumo casc")
                || !["--timeout", "--jobs", "--no-config", "--no-db"]
                    .iter()
                    .all(|flag| help.contains(flag))
            {
                return Err(
                    "The detected sumo does not support Sigma's required CASC CLI options"
                        .to_string(),
                );
            }
            Ok(version)
        };
        match tokio::time::timeout(Duration::from_secs(3), probe).await {
            Ok(Ok(version)) => prover.version = version,
            Ok(Err(message)) => prover.unavailable_reason = Some(message),
            Err(_) => prover.unavailable_reason = Some("sumo CLI discovery timed out".into()),
        }
    }
    provers
}
fn reported_status(prover: &str, text: &str) -> String {
    let status = szs_status(text);
    if prover != "supr" || status != "NoStatus" {
        return status;
    }
    // Some sumo harness-error paths print only a one-problem CASC summary.
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() == 4
            && fields[2] == "/"
            && fields[3] == "1"
            && ["Timeout", "GaveUp"].contains(&fields[0])
            && fields[1].parse::<usize>().is_ok()
        {
            return fields[0].into();
        }
    }
    status
}
pub fn local_provers() -> Vec<Prover> {
    let home = std::env::var("HOME").unwrap_or_default();
    vec![
        Prover {
            id: "e".into(),
            name: "E".into(),
            version: "3.2".into(),
            executable: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../data/tools/e-3.2/eprover"),
            env: vec![],
            unavailable_reason: None,
            args: vec![
                "--auto".into(),
                "--cpu-limit={seconds}".into(),
                "--tstp-format".into(),
                "--proof-object".into(),
                "{file}".into(),
            ],
        },
        Prover {
            id: "vampire".into(),
            name: "Vampire".into(),
            version: "5.0.1".into(),
            executable: Path::new(&home).join("Programs/vampire/build/vampire"),
            env: vec![],
            unavailable_reason: None,
            args: vec![
                "--mode".into(),
                "casc".into(),
                "--cores".into(),
                "1".into(),
                "--time_limit".into(),
                "{seconds}".into(),
                "{file}".into(),
            ],
        },
        Prover {
            id: "leo-iii".into(),
            name: "Leo-III".into(),
            version: "1.7.20".into(),
            executable: Path::new(&home).join("Programs/Leo-III/bin/leo3"),
            env: vec![],
            unavailable_reason: None,
            args: vec![
                "{file}".into(),
                "-t".into(),
                "{seconds}".into(),
                "-p".into(),
            ],
        },
        Prover {
            id: "supr".into(),
            name: "SUPr".into(),
            version: "sumo CLI (not probed)".into(),
            executable: sumo_path(Path::new(&home)),
            env: vec![("SIGMA_CORES".into(), "1".into())],
            unavailable_reason: None,
            args: vec![
                "--no-config".into(),
                "--no-db".into(),
                "casc".into(),
                "{file}".into(),
                "--timeout".into(),
                "{seconds}".into(),
                "--jobs".into(),
                "1".into(),
                "--quiet".into(),
            ],
        },
    ]
}

/// Execute one local process group. GNU time measures child CPU and peak RSS.
pub async fn execute(
    prover: &Prover,
    problem: &Problem,
    seconds: u64,
    folder: &Path,
    index: usize,
    cancelled: Arc<AtomicBool>,
) -> ResultRow {
    let stem = format!("{index:04}-{}", prover.id);
    let input = folder.join(format!("{stem}.p"));
    let output_file = format!("{stem}.log");
    let output = folder.join(&output_file);
    let metrics = folder.join(format!("{stem}.time"));
    let mut row = ResultRow {
        prover: prover.id.clone(),
        problem: problem.name.clone(),
        expected: problem.expected.clone(),
        state: "finished".into(),
        status: "ProcessError".into(),
        wall_seconds: 0.0,
        cpu_seconds: None,
        peak_rss_kb: None,
        exit_code: None,
        output_present: false,
        output_file,
        error: None,
    };
    let started = Instant::now();
    let execution: Result<(), String> = async {
        tokio::fs::write(&input, &problem.content)
            .await
            .map_err(|e| e.to_string())?;
        let stdout = std::fs::File::create(&output).map_err(|e| e.to_string())?;
        let stderr = stdout.try_clone().map_err(|e| e.to_string())?;
        let mut command = Command::new("/usr/bin/time");
        command
            .args(["-f", "%U %S %M", "-o"])
            .arg(&metrics)
            .arg("--")
            .arg("prlimit")
            .arg(format!("--cpu={}:{}", seconds + 1, seconds + 2))
            .arg("--fsize=33554432")
            .arg("--core=0")
            .arg("--")
            .arg(&prover.executable);
        for arg in &prover.args {
            command.arg(
                arg.replace("{seconds}", &seconds.to_string())
                    .replace("{file}", &input.to_string_lossy()),
            );
        }
        command.envs(prover.env.iter().map(|(key, value)| (key, value)));
        command
            .stdout(stdout)
            .stderr(stderr)
            .current_dir(folder)
            .env("OMP_NUM_THREADS", "1")
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        let pid = child.id().ok_or("Missing child process ID")?;
        let group = ProcessGroup(pid as i32);
        let deadline = sleep(Duration::from_secs(seconds + 5));
        tokio::pin!(deadline);
        let status = loop {
            tokio::select! {
                status=child.wait()=>break Some(status.map_err(|e|e.to_string())?),
                _=&mut deadline=>{row.status="Timeout".into();break None;},
                _=sleep(Duration::from_millis(50))=>{
                    if cancelled.load(Ordering::Relaxed) {row.status="Cancelled".into();break None;}
                }
            }
        };
        drop(group);
        if let Some(status) = status {
            row.exit_code = status.code();
            let text = tokio::fs::read_to_string(&output).await.unwrap_or_default();
            row.status = reported_status(&prover.id, &text);
            if row.status == "NoStatus" && !status.success() {
                row.status = "ProcessError".into();
                row.error = Some(format!("Prover exited with {status}"));
            }
            if prover.id == "supr" && row.status == "NoStatus" {
                row.error = Some("sumo did not report an SZS result".into());
            }
            row.output_present =
                text.contains("SZS output start") && text.contains("SZS output end");
        } else {
            let _ = child.wait().await;
        }
        if let Ok(text) = tokio::fs::read_to_string(&metrics).await
            && let Some(values) = text.lines().rev().find_map(|line| {
                let values: Vec<_> = line.split_whitespace().collect();
                if values.len() != 3 {
                    return None;
                }
                Some((
                    values[0].parse::<f64>().ok()?,
                    values[1].parse::<f64>().ok()?,
                    values[2].parse::<u64>().ok()?,
                ))
            })
        {
            row.cpu_seconds = Some(values.0 + values.1);
            row.peak_rss_kb = Some(values.2);
        }
        Ok(())
    }
    .await;
    if let Err(error) = execution {
        row.error = Some(error);
    }
    row.wall_seconds = started.elapsed().as_secs_f64();
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn captures_status_and_handles_missing_executable() {
        let folder = std::env::temp_dir().join(format!("atp-runner-{}", std::process::id()));
        tokio::fs::create_dir_all(&folder).await.unwrap();
        let mut p = Prover {
            id: "fixture".into(),
            name: "Fixture".into(),
            version: "test".into(),
            executable: "/usr/bin/printf".into(),
            env: vec![],
            unavailable_reason: None,
            args: vec!["%% SZS status Theorem\n".into()],
        };
        let problem = Problem {
            name: "test".into(),
            content: "fof(t,conjecture,p).".into(),
            expected: "Theorem".into(),
            source: None,
            library_id: None,
        };
        let r = execute(
            &p,
            &problem,
            1,
            &folder,
            0,
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        assert_eq!(r.status, "Theorem");
        assert!(r.matched());
        assert!(r.cpu_seconds.is_some());
        p.executable = "/not/a/prover".into();
        let r = execute(
            &p,
            &problem,
            1,
            &folder,
            1,
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        assert_eq!(r.status, "ProcessError");
        assert!(r.error.is_some());
        for file in std::fs::read_dir(&folder).unwrap() {
            std::fs::remove_file(file.unwrap().path()).unwrap();
        }
        std::fs::remove_dir(&folder).unwrap();
    }

    #[tokio::test]
    async fn timeout_and_cancellation_stop_process_groups() {
        let folder = std::env::temp_dir().join(format!("atp-control-{}", std::process::id()));
        tokio::fs::create_dir_all(&folder).await.unwrap();
        let p = Prover {
            id: "sleep".into(),
            name: "Sleep".into(),
            version: "test".into(),
            executable: "/usr/bin/sleep".into(),
            env: vec![],
            unavailable_reason: None,
            args: vec!["30".into()],
        };
        let problem = Problem {
            name: "test".into(),
            content: "fof(t,conjecture,p).".into(),
            expected: "Theorem".into(),
            source: None,
            library_id: None,
        };
        let started = Instant::now();
        let r = execute(
            &p,
            &problem,
            1,
            &folder,
            0,
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        assert_eq!(r.status, "Timeout");
        assert!(started.elapsed().as_secs() < 9);
        let started = Instant::now();
        let r = execute(
            &p,
            &problem,
            30,
            &folder,
            1,
            Arc::new(AtomicBool::new(true)),
        )
        .await;
        assert_eq!(r.status, "Cancelled");
        assert!(started.elapsed().as_secs() < 2);
        for file in std::fs::read_dir(&folder).unwrap() {
            std::fs::remove_file(file.unwrap().path()).unwrap();
        }
        std::fs::remove_dir(folder).unwrap();
    }
}

#[cfg(test)]
mod supr_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atp-supr-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    #[test]
    fn path_discovery_skips_non_executable_files_and_respects_availability() {
        let root = fixture();
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        for (dir, mode) in [(&first, 0o644), (&second, 0o755)] {
            let file = dir.join("sumo");
            std::fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(
            on_path("sumo", &path),
            Some(second.join("sumo").canonicalize().unwrap())
        );
        assert!(on_path("not-installed", &path).is_none());
        let mut prover = local_provers()
            .into_iter()
            .find(|p| p.id == "supr")
            .unwrap();
        prover.executable = second.join("sumo");
        assert!(prover.is_available());
        prover.unavailable_reason = Some("unsupported CLI".into());
        assert!(!prover.is_available());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cli_summary_fallback_only_classifies_single_problem_failures() {
        assert_eq!(
            reported_status("supr", "CASC summary: 0 / 1 solved\n Timeout 0 / 1\n"),
            "Timeout"
        );
        assert_eq!(reported_status("supr", "GaveUp 0 / 1\n"), "GaveUp");
        assert_eq!(reported_status("supr", "Theorem 1 / 1\n"), "NoStatus");
        assert_eq!(reported_status("supr", "Timeout 0 / 2\n"), "NoStatus");
        assert_eq!(reported_status("e", "Timeout 0 / 1\n"), "NoStatus");
        assert_eq!(
            reported_status("supr", "% SZS status Theorem for p\nTimeout 0 / 1\n"),
            "Theorem"
        );
    }
    #[tokio::test]
    async fn cli_receives_one_file_budget_and_isolated_configuration() {
        let root = fixture();
        let folder = root.join("path with spaces");
        std::fs::create_dir(&folder).unwrap();
        let script = root.join("fake-sumo");
        std::fs::write(&script,r#"#!/bin/sh
set -eu
test "$1" = --no-config
test "$2" = --no-db
test "$3" = casc
test -f "$4"
test "$5" = --timeout
test "$6" = 2
test "$7" = --jobs
test "$8" = 1
test "$9" = --quiet
test "$SIGMA_CORES" = 1
test "$OMP_NUM_THREADS" = 1
printf '%% SZS status Theorem for fixture\n%% SZS output start Proof\ncnf(done,plain,$false).\n%% SZS output end Proof\n'
"#).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut prover = local_provers()
            .into_iter()
            .find(|p| p.id == "supr")
            .unwrap();
        prover.executable = script;
        let problem = Problem {
            name: "fixture.p".into(),
            content: "fof(goal,conjecture,p=>p).".into(),
            expected: "Theorem".into(),
            source: None,
            library_id: None,
        };
        let row = execute(
            &prover,
            &problem,
            2,
            &folder,
            0,
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        assert_eq!(row.exit_code, Some(0));
        assert!(row.matched());
        assert!(row.output_present);
        assert!(row.cpu_seconds.is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
