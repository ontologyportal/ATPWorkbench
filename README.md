# ATPWorkbench

A workbench for automated theorem proving. ProverBench runs test suites
against native local ATP processes and records reproducible results.

## Start

Run these commands from Ubuntu / WSL:

```bash
cd ~/workspace/ATPWorkbench
npm install
npm run build:web
cargo run -p atp-server
```

Open http://localhost:7878. The Rust server serves the built Vue interface.
Keep the terminal open; Ctrl+C stops the server.

For frontend development, keep the Rust server running and use another terminal:

```bash
npm run web
```

Open http://localhost:5173. Vite proxies `/api` to Rust on port 7878.
Both listeners are restricted to loopback; this is a single-user local tool.

## First experiment

The interface includes a two-problem first-order smoke suite. Select the
installed adapters and click Run test suite. Every selected prover attempts
every problem sequentially, continuing after successful answers and errors.

Use Test suite to upload multiple self-contained `.p` / `.tptp` files, edit
inputs, and set expected SZS statuses. TPTP `% Status : ...` headers populate
expectations automatically. Unknown expectations do not count toward matches.
Limits: 100 problems, 1 MB per problem, 1-300 CPU-budget seconds per invocation.
Include directives are not supported; inline required axioms first.

Use Run history to reopen completed or interrupted experiments. CSV exports
contain every attempted prover/problem pair; JSON contains inputs, configuration,
results, environment metadata and standings. View log opens complete captured
stdout/stderr and supports downloading it. Files are saved in `data/runs/`.
Cancellation terminates the active process group and skips pending executions.

## Workspace layout

- `crates/core`: problem/result models, SZS parsing and aggregate statistics.
- `crates/runner`: native Linux process execution, timeouts and cancellation.
- `crates/server`: Axum API, run persistence and static web serving.
- `packages/web`: Vue, TypeScript and Vite, following sigma-rs conventions.
- `reference`: the exact 92-entry target catalogue and acquisition notes.

The sibling sigma-rs and online-atps repositories are not runtime dependencies.

## Local adapters

The initial runner explicitly configures E 3.2, Vampire 5.0.1 and Leo-III 1.7.20.
Their paths and arguments are in `crates/runner/src/lib.rs`. E's release binary
was copied into `data/tools/e-3.2/eprover`; Vampire and Leo-III use the existing
installations under `~/Programs`. Other machines must install/configure these
binaries; they are not committed to this repository. A missing binary is shown
as unavailable. No download or remote-execution fallback occurs at run time.

The 92 reference entries are NOT installed or certified as exact reproductions.
No entry is certified merely because its name/version appears in the registry.

## Statistics and CASC differences

Reference: https://tptp.org/CASC/J13/Design.html#Evaluation

This is a CASC-inspired exploratory benchmark, not official CASC scoring:

- Standings rank expected-status matches, then average CPU time on matching
  answers, then average elapsed wall time.
- Definitive SZS answers must exactly match the expected status. Wrong answers,
  unknown expectations, errors, timeouts and cancellation do not count.
- A proof/model output block is detected but is NOT checked for correctness,
  syntax, structure or acceptance. Matches are unverified prover claims.
- Unique matches count problems matched by only one selected prover.
- SotAC averages `(selected systems - matching systems) / selected systems`
  over a system's matched problems that were not matched by every selected
  system. This adapts the CASC-J13 definition to unverified expected matches;
  no historical-winner exclusions are applied. Partial/cancelled suites have
  provisional statistics.
- The performance profile shows cumulative CPU time for matching problems.
- No CASC division eligibility filtering, independent proof/model checking,
  solution-time timestamps, proof granularity or controlled competition
  hardware is implemented yet. Avoid mixing incompatible logics when comparing.

GNU `/usr/bin/time` records user+system CPU time and maximum RSS, not summed
concurrent memory. CPU values have centisecond resolution: very quick jobs may
report 0.00. Wall time includes process startup, execution and output completion;
it is not the timestamp of the first SZS answer. CPU/RSS can be unavailable for
force-killed runs and remain null rather than being fabricated as zero.

Linux `prlimit` supplies per-process CPU limits (one-second soft-limit grace,
two-second hard-limit grace), a 32 MB output-file limit, and disables core dumps.
A wall-clock timeout of requested budget + 5 seconds terminates the process
group. This is not aggregate cgroup CPU accounting, CPU pinning, or a hard memory
limit. External helper processes may affect comparisons. Installed binaries
are trusted local software; this is not a sandbox for arbitrary binaries.
The service itself has no remote prover backend, but OS network isolation is
not yet enforced on child processes.

## Checks

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
npm run build:web
```

## API

- GET `/api/health`, `/api/provers`, `/api/batches`
- POST `/api/batches`: `{name, seconds, problems: [{name, content, expected}], provers: [ids]}`
- GET `/api/batches/{id}`: initial job details, paginated results and whole-job standings; `?compact=true&offset=100` omits descriptors and selects a result page
- POST `/api/batches/{id}/cancel`
- GET `/api/batches/{id}/output/{index}`
- GET `/api/batches/{id}/export.csv`, `/api/batches/{id}/export.json`
- GET `/api/batches/{id}/input/{index}`
- POST `/api/library/import`: `{ids: [...]}` returns lightweight problem references
- POST `/api/library/preview`: `{id: "Problems/PUZ/PUZ001+1.p"}` loads one input


## Local TPTP library

Set `ATP_TPTP_ROOT` to a local TPTP root containing `Problems/` and `Axioms/`, or create `data/library.json` with `{"root":"/path/to/TPTP-v9.3.0"}`. Restart the server after changing the path. Under WSL a Windows folder can be addressed as `/mnt/c/Users/shaun/OneDrive/Desktop/TPTP-v9.3.0`. The configured folder is read in place; it is not copied into the repository.

Open **Test suite → TPTP library**, filter by filename/domain/language, expand domain directories, select problems and click **Add selected**. Directory checkboxes select every `.p` file in the directory, across all pages and regardless of active filters; partially selected directories show an indeterminate checkbox. Selections survive collapsing directories or changing filters. The table header checkbox selects all library problems. A job can contain the entire selection; there is no 100-problem cap. Clear the smoke examples if you want a suite containing only imported problems. Importing does not start any provers. Expected SZS statuses come from TPTP headers; unrecognized expectations become Unknown. Language and domain filters describe the input, not which installed adapters support it. The full library is not a specific CASC competition selection.

Selections add lightweight library references rather than copying expanded inputs into the browser. A single job can run the full library. The worker loads **one problem at a time**, reads its expected status and expands recursive `include` directives (including formula selections), snapshots the exact input and SHA-256 hash, runs every selected prover, then moves on. Unreadable, unsupported or oversized inputs produce InputError rows and do not stop the rest of the job. Imports and previews do not start provers.

The input expansion budget is 64 MiB per library problem including source dependencies. This is a per-input memory bound, not a job-size limit. Custom uploads remain self-contained and limited to 1 MiB per file, with a 12 MiB request limit. Access stays confined to the configured library with cycle/depth checks. Since library input is read on demand, edits to a source file before the worker reaches it affect that execution; the saved snapshot records exactly what was used. Use an unchanged versioned library for comparison runs. Preview library inputs on demand; converting a preview to a custom editable copy removes its library reference.

New jobs keep lightweight descriptors in `data/runs/<id>/batch.json`, append and flush each result to `results.jsonl`, and store expanded problem snapshots in `inputs/<index>.json` plus hashes/source metadata in `inputs.jsonl`. Progress requests omit the full problem list and return at most 100 result rows with whole-job standings and a sampled performance profile. The suite editor displays 20 problem descriptors per page. Full CSV and JSON exports include every recorded execution, regardless of the visible page; JSON also includes the input snapshot index. Individual input snapshots are available through `GET /api/batches/{id}/input/{index}`. Old saved runs are still readable. After a server interruption, completed journal records are recovered and the job is marked interrupted; automatic resume is not implemented. Cancellation stops the current process group and leaves completed records available. Proof/model acceptance remains unvalidated.

For isolated local checks, `ATP_PORT` overrides the listening port and `ATP_STORAGE_ROOT` changes the parent of `data/runs` without changing the library configuration or web assets. Defaults remain port 7878 and the repository directory. Do not point two server instances at the same run-storage root.

While a job is running, the interface shows the worker-reported current problem, prover, problem/execution positions and elapsed wall time. The live card distinguishes input preparation, prover execution and result saving; it updates from the API every two seconds. `batch.current_execution` is transient and becomes null when the job stops, so restarting the server never restores a stale active problem.

## Optional SUPr adapter

Install SigmaKEE's `sumo` CLI in the WSL/Linux environment that runs the backend. At startup ATPWorkbench searches its PATH, falling back to `~/.sigmakee/bin/sumo`, and probes `sumo --version` and `sumo casc --help` with a bounded timeout. Missing, non-executable or incompatible installations stay unavailable in the registry. Restart the backend after installing or replacing sumo; the Sigma repository is not required at runtime.

SUPr is invoked once per problem with `sumo --no-config --no-db casc <file> --timeout <seconds> --jobs 1 --quiet`. `SIGMA_CORES=1` requests one native strategy core; `--jobs 1` independently limits concurrent problem jobs. Sigma config/database loading and the CLI update notice/check are disabled, and ATPWorkbench supplies its self-contained input snapshot. Existing process-group cancellation, CPU/wall limits, log capture and measurement apply. The detected full CLI version (including commit when reported), executable, arguments and adapter environment are recorded in the run manifest. This is an installed-CLI adapter, not a claim of exact CASC catalogue reproduction.
