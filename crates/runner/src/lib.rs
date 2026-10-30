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
            args: vec![
                "{file}".into(),
                "-t".into(),
                "{seconds}".into(),
                "-p".into(),
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
            row.status = szs_status(&text);
            if row.status == "NoStatus" && !status.success() {
                row.status = "ProcessError".into();
                row.error = Some(format!("Prover exited with {status}"));
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
            args: vec!["%% SZS status Theorem\n".into()],
        };
        let problem = Problem {
            name: "test".into(),
            content: "fof(t,conjecture,p).".into(),
            expected: "Theorem".into(),
            source: None,
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
            args: vec!["30".into()],
        };
        let problem = Problem {
            name: "test".into(),
            content: "fof(t,conjecture,p).".into(),
            expected: "Theorem".into(),
            source: None,
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
