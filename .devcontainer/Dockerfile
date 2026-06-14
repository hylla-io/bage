# syntax=docker/dockerfile:1

# bage polyglot dev image
#
# Every toolchain + language server bage drives, so the full
# create / show / apply / rename / diagnose matrix runs inside a container with
# NOTHING installed on the host. This same image backs the VS Code
# "Reopen in Container" flow via .devcontainer/devcontainer.json, and is what the
# orchestrator builds + `docker exec`s into to drive bage on the mounted repo.
#
# Language servers included: gopls (Go), rust-analyzer (Rust), pyright (Python),
# typescript-language-server (TS/TSX/JS/JSX), clangd (C/C++).
# Java/C#/Swift LSPs are intentionally omitted here (heavy to install); their
# real-LSP rename is covered by the testcontainers matrix (`mage lsp`). Those
# langs are still exercised for create/show/apply/diagnose through bage's
# built-in tree-sitter grammars — no server required.

FROM debian:bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive \
    GOPATH=/root/go \
    PATH=/usr/local/go/bin:/root/go/bin:/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin

# --- base tooling + clangd (C/C++ LSP) + python + node ---
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git xz-utils build-essential pkg-config \
        clangd \
        python3 python3-pip \
        nodejs npm \
    && rm -rf /var/lib/apt/lists/*

# --- Go toolchain + gopls (Go LSP) + mage (builds bage in-container) ---
ARG GO_VERSION=1.24.0
RUN arch="$(dpkg --print-architecture)" \
    && curl -fsSL "https://go.dev/dl/go${GO_VERSION}.linux-${arch}.tar.gz" -o /tmp/go.tgz \
    && tar -C /usr/local -xzf /tmp/go.tgz \
    && rm /tmp/go.tgz \
    && go install golang.org/x/tools/gopls@latest \
    && go install github.com/magefile/mage@latest

# --- Rust + rust-analyzer (Rust LSP) ---
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --component rust-analyzer

# --- Python (pyright) + TypeScript language servers, via npm globals ---
RUN npm install -g pyright typescript typescript-language-server

WORKDIR /workspace

# Keep the container alive so the orchestrator can `docker exec` bage commands at
# its own pace (one e2e step at a time) against the mounted repo.
CMD ["sleep", "infinity"]
