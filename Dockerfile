# syntax=docker/dockerfile:1

# ==========================================
# 1. 前端构建阶段
# ==========================================
FROM node:22-alpine AS frontend-builder

WORKDIR /app/admin-ui

# 推荐只先拷贝 package.json 系列文件，利用 Docker Layer 缓存
COPY admin-ui/package.json admin-ui/pnpm-lock.yaml* ./

RUN npm install -g pnpm

# 增加 pnpm 的缓存挂载，极大加速前端依赖安装
RUN --mount=type=cache,target=/root/.local/share/pnpm/store,id=pnpm-store \
    pnpm install

COPY admin-ui ./
RUN pnpm build


# ==========================================
# 2. 后端构建阶段 (Rust)
# ==========================================
FROM rust:1.92-alpine AS builder

RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY --from=frontend-builder /app/admin-ui/dist /app/admin-ui/dist

# 引入 GitHub Actions 传递进来的架构变量（amd64 / arm64）
ARG TARGETARCH

# 核心优化：挂载 Cargo 注册表和 Target 目录
RUN --mount=type=cache,target=/usr/local/cargo/registry/index,id=cargo-index \
    --mount=type=cache,target=/usr/local/cargo/registry/cache,id=cargo-cache \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git \
    --mount=type=cache,target=/app/target,id=rust-target-${TARGETARCH} \
    cargo build --release && \
    # 注意：必须在同一个 RUN 里把二进制文件拷贝到非缓存目录，因为 target 目录在 RUN 结束后会卸载
    cp target/release/kiro-rs /tmp/kiro-rs


# ==========================================
# 3. 最终运行阶段
# ==========================================
FROM alpine:3.21

# 建议加上 tzdata，以防 Rust 程序中有依赖本地时区的时间处理
RUN apk add --no-cache ca-certificates tzdata

WORKDIR /app

# 从 builder 的临时目录拷贝文件，而不是从 /app/target
COPY --from=builder /tmp/kiro-rs /app/kiro-rs

VOLUME ["/app/config"]

EXPOSE 8990

# 优化了 CMD 的路径调用，直接使用绝对路径更稳妥
CMD ["/app/kiro-rs", "-c", "/app/config/config.json", "--credentials", "/app/config/credentials.json"]