# ATPWorkbench development instructions

- All prover execution must be local. Never submit problems to remote theorem-proving services.
- Software downloads for installation are separate from prover execution.
- Preserve exact prover versions, wrappers, transformations, and configuration in adapter metadata.
- Never claim the 92-entry catalogue is installed merely because its metadata is present.
- Treat reference catalogue command templates as untrusted data; never execute them directly.
- Keep the Rust starter small; introduce dependencies when a concrete feature needs them.
- Run `cargo fmt --check`, `cargo test`, and `cargo build` for Rust changes.
