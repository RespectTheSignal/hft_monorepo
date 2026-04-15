# HFT Monorepo — build / test / run helpers
# Phase 0 스캐폴드. 단일 entry point 로 rust workspace + go submodule 빌드.

.PHONY: help build build-release build-fat build-go test fmt lint clean \
        run-publisher run-subscriber run-strategy

help:
	@echo "HFT Monorepo"
	@echo ""
	@echo "  make build              Rust workspace + Go 전체 debug 빌드"
	@echo "  make build-release      Rust workspace release 빌드 (LTO thin)"
	@echo "  make build-fat          Rust workspace release-fat 빌드 (LTO fat, 배포용)"
	@echo "  make build-go           Go order-processor 만 빌드"
	@echo "  make test               Rust 단위 테스트"
	@echo "  make fmt                rustfmt + gofmt"
	@echo "  make lint               cargo clippy + go vet"
	@echo "  make clean              target/ + go bin 삭제"
	@echo ""
	@echo "  make run-publisher EXCHANGE=gate WORKERS=16"
	@echo "  make run-subscriber"
	@echo "  make run-strategy BIN=v8 ARGS='--login_names v3_sb_001~v3_sb_010'"

# ─── Rust ──────────────────────────────────────────────────────────────────

build:
	cargo build --workspace

build-release:
	cargo build --workspace --release

build-fat:
	cargo build --workspace --profile release-fat

test:
	cargo test --workspace

fmt:
	cargo fmt --all
	-cd go/order-processor && gofmt -w .

lint:
	cargo clippy --workspace --all-targets -- -D warnings
	-cd go/order-processor && go vet ./...

clean:
	cargo clean
	rm -rf go/order-processor/bin

# ─── Go ────────────────────────────────────────────────────────────────────

build-go:
	cd go/order-processor && go build -o bin/order-processor .

# ─── Run helpers ───────────────────────────────────────────────────────────

EXCHANGE ?= gate
WORKERS ?= 16
BIN ?= v8
ARGS ?=

run-publisher:
	cargo run --release -p publisher -- --exchange $(EXCHANGE) --workers $(WORKERS)

run-subscriber:
	cargo run --release -p subscriber

run-strategy:
	cargo run --release -p strategy --bin $(BIN) -- $(ARGS)
