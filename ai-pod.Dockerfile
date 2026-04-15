FROM rust:latest AS builder

#ARG AI_POD_VERSION
#RUN ARCH=$(uname -m) && \
#    curl -fsSL "https://github.com/mismosmi/ai-pod/releases/download/v${AI_POD_VERSION}/host-tools-linux-${ARCH}" \
#      -o /usr/local/bin/host-tools && chmod +x /usr/local/bin/host-tools

WORKDIR /app

COPY . .

RUN ls

RUN cargo build --release

FROM rust:latest
WORKDIR /app

COPY --from=builder /app/target/release/host-tools /usr/local/bin/host-tools

RUN host-tools install claude

RUN useradd -u 1000 -ms /bin/bash claude
RUN chown -R claude /app

# System-level git identity
RUN git config --system user.email "claude@ai-pod" && \
    git config --system user.name "claude"

USER claude

ENV PATH="/home/claude/.local/bin:${PATH}"


CMD ["claude"]
