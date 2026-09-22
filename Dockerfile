# ==========================================
# 阶段 1：构建编译（Rust release 二进制）
# ==========================================
FROM rust:slim-bookworm AS builder

WORKDIR /app

# 安装基础编译工具
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# 依赖源镜像（默认走国内镜像，避免直连 crates.io 超时；海外可传 --build-arg CARGO_MIRROR= 关闭）
ARG CARGO_MIRROR=sparse+https://rsproxy.cn/index/
RUN if [ -n "$CARGO_MIRROR" ]; then \
        printf '[source.crates-io]\nreplace-with = "mirror"\n\n[source.mirror]\nregistry = "%s"\n\n[net]\nretry = 5\ngit-fetch-with-cli = true\n' "$CARGO_MIRROR" > /usr/local/cargo/config.toml && \
        echo "cargo mirror: $CARGO_MIRROR"; \
    else \
        echo "cargo mirror: disabled (using official crates.io)"; \
    fi

# 先只复制依赖清单，用空壳源码预编译依赖（利用 Docker 层缓存加速后续构建）
COPY Cargo.toml Cargo.lock ./
RUN mkdir src tests && \
    echo "fn main() {}" > src/main.rs && \
    echo "pub fn dummy() {}" > src/lib.rs && \
    cargo build --release && \
    rm -rf src tests

# 复制真实源码与编译期嵌入资源（static 由 rust_embed 在编译期打包进二进制）
COPY src ./src
COPY tests ./tests
COPY static ./static
COPY config ./config
COPY prompt_engineering ./prompt_engineering
COPY experience ./experience

# 触碰源码使_dummy 构建产物失效，编译真实 release 二进制
RUN touch src/main.rs src/lib.rs && \
    cargo build --release --bin mt5-2pa-agent

# ==========================================
# 阶段 2：最小化生产运行时
# ==========================================
FROM debian:bookworm-slim AS runner

# ca-certificates 用于访问 LLM API 的 HTTPS；tzdata 用于时区
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    tzdata \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# 默认时区上海（可通过 TZ 环境变量覆盖）
ENV TZ=Asia/Shanghai \
    RUST_LOG=info

# 复制编译产物
COPY --from=builder /app/target/release/mt5-2pa-agent /app/mt5-2pa-agent

# 复制运行时需要从磁盘读取的目录：
#   - prompt_engineering/  策略提示词（按行情状态动态加载）
#   - experience/          经验库（注入 Stage2 提示词）
#   - config/              settings.json 默认配置（.env 由卷挂载注入）
COPY --from=builder /app/prompt_engineering ./prompt_engineering
COPY --from=builder /app/experience ./experience
COPY --from=builder /app/config ./config

# 确保持久化目录存在（records 建议通过卷挂载持久化）
RUN mkdir -p /app/records /app/records/pending

EXPOSE 8066

# 容器内必须监听 0.0.0.0，否则外部（MT5 所在 Windows 机器）无法访问
ENTRYPOINT ["/app/mt5-2pa-agent", "--host", "0.0.0.0", "--port", "8066"]
