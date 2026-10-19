.PHONY: check-normal  perf-test

check-normal:
	cargo fmt --check
	cargo check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test

perf-test:
	cargo test --release --test perf_test -- --ignored --nocapture
	cargo test --release --test load_test -- --ignored