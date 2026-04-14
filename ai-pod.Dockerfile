FROM rust:latest

ARG AI_POD_VERSION
RUN ARCH=$(uname -m) && \
    curl -fsSL "https://github.com/mismosmi/ai-pod/releases/download/v${AI_POD_VERSION}/host-tools-linux-${ARCH}" \
      -o /usr/local/bin/host-tools && chmod +x /usr/local/bin/host-tools

RUN host-tools install

WORKDIR /app

RUN useradd -ms /bin/bash claude
RUN chown -R claude /app

# System-level git identity
RUN git config --system user.email "claude@ai-pod" && \
    git config --system user.name "claude"

USER claude

ENV PATH="/home/claude/.local/bin:${PATH}"


CMD ["claude"]
