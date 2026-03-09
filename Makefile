.PHONY: dev release test test-rust test-python clean

# Install in development mode (debug build, into current venv)
dev:
	maturin develop

# Install in development mode (optimized release build)
release:
	maturin develop --release

# Build a wheel (.whl) for distribution
wheel:
	maturin build --release

# Run all tests
test: test-rust test-python

# Rust unit tests (no Python interpreter needed)
test-rust:
	cargo test

# Python tests (requires `maturin develop` first)
test-python:
	python -m pytest tests/ -v

clean:
	cargo clean
	rm -rf target/ dist/
