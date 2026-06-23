FROM rust:latest

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git vim && rm -rf /var/lib/apt/lists/*

ARG HOST_GATEWAY
ARG AI_POD_VERSION
RUN curl -fsSL "http://${HOST_GATEWAY}:7822/install/claude.sh" | bash
RUN curl -fsSL "http://${HOST_GATEWAY}:7822/install/opencode.sh" | bash

WORKDIR /app

RUN useradd -ms /bin/bash ai-pod && chown -R ai-pod /app

# System-level git identity (fallback when no host identity is provided)
RUN git config --system user.email "git@michelsmola.de" && \
    git config --system user.name "Michel Smola"

USER ai-pod

ENV PATH="/home/ai-pod/.local/bin:${PATH}"
ENV EDITOR=vim

CMD ["claude"]
