ci: fmt-check clippy test leaks

fmt-check:
    cargo fmt --all -- --check

clippy:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

# Secrets scan plus a local deny-list of strings that must never appear in this
# public repository. The deny-list lives outside the repo (it names the private
# infrastructure itself); set MR_DENYLIST to its path to enable the check.
leaks:
    gitleaks git --no-banner --redact .
    @if [ -n "${MR_DENYLIST:-}" ]; then ! git grep --untracked -nIiE -f "$MR_DENYLIST" -- . ; fi

run:
    cargo run
