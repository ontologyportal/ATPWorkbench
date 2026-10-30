pub mod tptp;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Problem {
    pub name: String,
    pub content: String,
    pub expected: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResultRow {
    pub prover: String,
    pub problem: String,
    pub expected: String,
    pub state: String,
    pub status: String,
    pub wall_seconds: f64,
    pub cpu_seconds: Option<f64>,
    pub peak_rss_kb: Option<u64>,
    pub exit_code: Option<i32>,
    pub output_present: bool,
    pub output_file: String,
    pub error: Option<String>,
}

impl ResultRow {
    pub fn matched(&self) -> bool {
        definitive(&self.expected) && self.state == "finished" && self.status == self.expected
    }
    pub fn wrong(&self) -> bool {
        definitive(&self.expected) && definitive(&self.status) && self.status != self.expected
    }
}

pub fn definitive(status: &str) -> bool {
    matches!(
        status,
        "Theorem" | "Unsatisfiable" | "Satisfiable" | "CounterSatisfiable"
    )
}

pub fn szs_status(output: &str) -> String {
    output
        .lines()
        .filter_map(|line| {
            let (_, tail) = line.split_once("SZS status ")?;
            tail.split_whitespace().next().map(str::to_string)
        })
        .next_back()
        .unwrap_or_else(|| "NoStatus".into())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Standing {
    pub prover: String,
    pub completed: usize,
    pub matches: usize,
    pub claims: usize,
    pub wrong: usize,
    pub timeouts: usize,
    pub errors: usize,
    pub output_count: usize,
    pub unique_matches: usize,
    pub average_cpu: Option<f64>,
    pub average_wall: Option<f64>,
    pub maximum_rss_mb: Option<f64>,
    pub sotac: f64,
}

/// Rank expected-status matches, then average CPU time, then wall time.
/// Proof/model acceptance is not implied by a matching status.
pub fn standings(rows: &[ResultRow], provers: &[String]) -> Vec<Standing> {
    let mut result: Vec<_> = provers
        .iter()
        .map(|prover| {
            let mine: Vec<_> = rows.iter().filter(|r| &r.prover == prover).collect();
            let matches: Vec<_> = mine.iter().filter(|r| r.matched()).collect();
            let cpus: Vec<_> = matches.iter().filter_map(|r| r.cpu_seconds).collect();
            let average = |values: &[f64]| {
                (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
            };
            let counts: Vec<_> = matches
                .iter()
                .map(|r| {
                    rows.iter()
                        .filter(|other| other.problem == r.problem && other.matched())
                        .count()
                })
                .collect();
            let contributions: Vec<_> = counts
                .iter()
                .filter(|&&n| n < provers.len())
                .map(|&n| (provers.len() - n) as f64 / provers.len() as f64)
                .collect();
            Standing {
                prover: prover.clone(),
                completed: mine.iter().filter(|r| r.state == "finished").count(),
                matches: matches.len(),
                claims: mine.iter().filter(|r| definitive(&r.status)).count(),
                wrong: mine.iter().filter(|r| r.wrong()).count(),
                timeouts: mine.iter().filter(|r| r.status == "Timeout").count(),
                errors: mine.iter().filter(|r| r.error.is_some()).count(),
                output_count: mine.iter().filter(|r| r.output_present).count(),
                unique_matches: counts.iter().filter(|&&n| n == 1).count(),
                average_cpu: average(&cpus),
                average_wall: average(&matches.iter().map(|r| r.wall_seconds).collect::<Vec<_>>()),
                maximum_rss_mb: mine
                    .iter()
                    .filter_map(|r| r.peak_rss_kb)
                    .max()
                    .map(|kb| kb as f64 / 1024.0),
                sotac: average(&contributions).unwrap_or(0.0),
            }
        })
        .collect();
    result.sort_by(|a, b| {
        b.matches
            .cmp(&a.matches)
            .then_with(|| {
                a.average_cpu
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&b.average_cpu.unwrap_or(f64::INFINITY))
            })
            .then_with(|| {
                a.average_wall
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&b.average_wall.unwrap_or(f64::INFINITY))
            })
            .then_with(|| a.prover.cmp(&b.prover))
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(p: &str, problem: &str, status: &str, cpu: f64) -> ResultRow {
        ResultRow {
            prover: p.into(),
            problem: problem.into(),
            expected: "Theorem".into(),
            state: "finished".into(),
            status: status.into(),
            wall_seconds: cpu,
            cpu_seconds: Some(cpu),
            peak_rss_kb: Some(1024),
            exit_code: Some(0),
            output_present: false,
            output_file: String::new(),
            error: None,
        }
    }
    #[test]
    fn parser_uses_last_status_not_proof_text() {
        assert_eq!(
            szs_status("% SZS status Started\n% SZS status Theorem for X\n"),
            "Theorem"
        );
        assert_eq!(szs_status("Proof found"), "NoStatus");
    }
    #[test]
    fn ranking_excludes_wrong_unknown_and_timeouts() {
        let mut rows = vec![
            row("a", "p1", "Theorem", 2.0),
            row("b", "p1", "Theorem", 1.0),
            row("a", "p2", "Theorem", 4.0),
            row("b", "p2", "Timeout", 5.0),
            row("b", "p3", "CounterSatisfiable", 0.1),
        ];
        rows.push(row("a", "p3", "Unknown", 0.2));
        let stats = standings(&rows, &["a".into(), "b".into()]);
        assert_eq!(stats[0].prover, "a");
        assert_eq!(stats[0].matches, 2);
        assert_eq!(stats[0].average_cpu, Some(3.0));
        assert_eq!(stats[0].unique_matches, 1);
        assert_eq!(stats[0].sotac, 0.5);
        assert_eq!(stats[1].wrong, 1);
        assert_eq!(stats[1].timeouts, 1);
        assert_eq!(stats[0].output_count, 0);
    }
    #[test]
    fn cpu_breaks_ties_and_empty_is_safe() {
        let rows = vec![row("a", "p", "Theorem", 2.0), row("b", "p", "Theorem", 1.0)];
        assert_eq!(standings(&rows, &["a".into(), "b".into()])[0].prover, "b");
        assert!(standings(&[], &[]).is_empty());
    }
}
