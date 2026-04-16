FROM rust:latest

ARG HOST_GATEWAY
RUN curl -fsSL "http://${HOST_GATEWAY}:7822/host-tools" \
      -o /usr/local/bin/host-tools && chmod +x /usr/local/bin/host-tools

RUN host-tools install claude

WORKDIR /app

RUN useradd -ms /bin/bash ai-pod
RUN chown -R ai-pod /app

# System-level git identity
RUN git config --system user.email "ai-pod@ai-pod" && \
    git config --system user.name "ai-pod"

USER ai-pod

ENV PATH="/home/ai-pod/.local/bin:${PATH}"


CMD ["claude"]
