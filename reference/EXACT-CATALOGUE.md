# Exact 92-entry reproduction: incomplete

The requested target is the exact 92 entries in the SystemOnTPTP form snapshot
observed on 2026-10-29, including the versions and distinct configurations.
`catalogue.json` records the system IDs, versions, advertised invocation
templates, input formats, transformations, and application descriptions.
These are reference data, never executable configuration.

No entry is currently certified as an exact reproduction of its online setup.
The exploratory local adapters in the dashboard are separate from this target.
For example, E 3.2 is not E 3.5.1, cvc5 1.3.4 is not cvc5 1.4.2, and a generic
Vampire invocation is not a reproduction of each of the Vampire variants.

## Missing upstream material

The catalogue exposes wrapper names such as `run_E`, not their implementations
or the dependency paths, build options, transformation programs, strategy
schedules, and all auxiliary files. Matching a version number alone cannot
establish equivalence.

The official documentation describes SystemOnTPTP as a local command-line tool
in the TPTPWorld `SystemExecution` directory:
https://www.cs.miami.edu/home/geoff/Courses/TPTPSYS/Practicum/UsingATPSystems.shtml

The documented package link is `https://tptp.org/TPTPWorld.tgz`:
https://tptp.org/Seminars/SystemOnTPTP/Utility/Summary.html

On 2026-10-29 that package URL returned HTTP 404. The public TPTPWorld GitHub
organization repositories inspected did not provide a complete `SystemExecution`
plus `Systems` installation matching this form. This does not establish that
the packages are unavailable elsewhere; a current distribution location or
upstream package export is needed before claiming exact reproduction.

## Suggested request to the maintainer (not sent)

Subject: Local reproduction of the 92-entry SystemOnTPTP catalogue

I would like to run the 92 entries from the SystemOnTPTP catalogue observed on
2026-10-29 entirely on my own Ubuntu x86_64 machine, rather than submit batch
jobs to your public service. Is there a current download for the matching
SystemExecution tools, SystemInfo configuration, Systems packages, run_*
wrappers, and required transformation utilities? The documented TPTPWorld.tgz
link currently returns 404. A manifest of package URLs and checksums would
also help. Please indicate any entries requiring separate author permission
or unavailable packages.

## Verification required per entry

1. Acquire the exact package/version and identify its provenance.
2. Install its dependencies, wrapper, and advertised transformation.
3. Record a hash of the package and effective local command.
4. Run suitable local syntax, positive, and negative examples for its logic.
5. Verify time limits and output capture before enabling that exact entry.

No remote comparison jobs have been submitted during this local setup work.
