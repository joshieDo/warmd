warmd keeps each repo in repos.toml built at main and checks patches against it through a job API; profiles.toml holds extra lint sets.
CI (.github/workflows/ci.yml) must pass: cargo fmt, clippy with slapslop's recommended set (Cargo.toml lints plus the non-test step), latest slapslop --all-lints, and the e2e test.
Fix every lint; only false positives get exceptions (.slapslop/config.toml or #[expect] with a reason).
No unit tests, only e2e (tests/e2e.rs, needs bwrap 0.11+). No documentation: code is documentation.
