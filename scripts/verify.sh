#!/bin/sh
# Verificação completa do projeto. Nada aqui toca áudio, dispositivos,
# teclado ou daemon real: os testes usam o agente simulado (sexta-mock-agente).
set -e
cd "$(dirname "$0")/.."

echo "== cargo fmt --check =="
cargo fmt --check

echo "== cargo check =="
cargo check --all-targets

echo "== cargo test =="
cargo test

echo "== cargo clippy =="
cargo clippy --all-targets -- -D warnings

echo "== cargo build --release =="
cargo build --release

echo "verificação completa: OK"
