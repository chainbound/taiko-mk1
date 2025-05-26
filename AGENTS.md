# Agent Guidelines for Mk1

## Test, Lint and Formatting Commands
- Run tests: `just test`
- Run single test: `cargo nextest run <test_name>`
- Linting: `just lint`
- Format code: `just fmt`
- Run all CI checks: `just ci` (runs fmt, lint, test)

## Code Style Guidelines
- Use Rust 2024 edition
- Follow rustfmt.toml settings: reordered imports, grouped by crate, use small heuristics
- Missing debug impls and docs should be warned
- Follow Clippy lints defined in Cargo.toml
- Errors use `thiserror` crate
- Tests: Use `#[cfg(test)]` module and functions with `#[test]` or `#[tokio::test]`
- Async: Use tokio for async runtime
- Prefer `#[derive(Debug)]` on structs and enums
- Use trace/debug/info/warn/error logs properly with `tracing` crate
- Error handling: Prefer `?` operator with contextual error info

## Git
- Use Conventional Commits for commits
