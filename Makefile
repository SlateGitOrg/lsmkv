# One command gates everything: `make check`.
CARGO ?= cargo
COVERAGE_MIN ?= 80
# Coverage measures the library (the engine); the CLI binaries and the demo
# are exercised by `make demo` / `make bench` instead.
COVERAGE_IGNORE = '(src/bin/|examples/|/tests\.rs$$)'

.PHONY: check fmt fmt-check lint typecheck test doctest coverage demo bench clean

check: fmt-check lint typecheck coverage doctest
	@echo "make check: all gates passed"

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --all-targets -- -D warnings

typecheck:
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --no-deps --quiet
	$(CARGO) check --all-targets --quiet

# Plain test run (no instrumentation).
test:
	$(CARGO) test

doctest:
	$(CARGO) test --doc --quiet

# Runs the whole test suite (unit, integration, crash, property) under
# instrumentation and fails below COVERAGE_MIN percent line coverage.
coverage:
	$(CARGO) llvm-cov --lib --tests --ignore-filename-regex $(COVERAGE_IGNORE) \
		--fail-under-lines $(COVERAGE_MIN) --summary-only

demo:
	$(CARGO) run --release --quiet --example demo

bench:
	./scripts/bench.sh

clean:
	$(CARGO) clean
	rm -rf demo-data bench/tmp
