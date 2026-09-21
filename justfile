# use PowerShell instead of sh:
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

all: check check-fmt clippy test

clippy:
  cargo clippy

fmt:
  cargo fmt

check-fmt:
  cargo fmt --all -- --check

check:
  cargo check
  cargo check --no-default-features
  cargo check --all-features

test:
  cargo nextest run --all-features

msrv:
  cargo +1.95 check --all-features

# Stripped server binary embedded into the pyprobe-rs extension.
build-serve-bin:
    cargo build --profile server -p probe-rs-tools --bin probe-rs

# Rebuild the Python extension (with embedded server) into the repo venv.
pyprobe: build-serve-bin
    cd pyprobe-rs && ../.venv/bin/python -m maturin develop
