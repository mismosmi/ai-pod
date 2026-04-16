FROM ubuntu:24.04

RUN apt-get update && apt-get install -y curl git vim

ARG HOST_GATEWAY
RUN curl -fsSL "http://${HOST_GATEWAY}:7822/host-tools" \
      -o /usr/local/bin/host-tools && chmod +x /usr/local/bin/host-tools

RUN host-tools install {{AGENT}}

WORKDIR /app

# System-level git identity (fallback when no host identity is provided)
RUN git config --system user.email "{{AGENT}}@ai-pod" && \
    git config --system user.name "{{AGENT}}"

USER ubuntu

ENV PATH="/home/ubuntu/.local/bin:${PATH}"
ENV EDITOR=vim

CMD ["{{AGENT}}"]
