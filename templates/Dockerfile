FROM ubuntu:24.04

RUN apt-get update && apt-get install -y curl git vim

ARG AI_POD_VERSION
RUN ARCH=$(uname -m) && \
    curl -fsSL "https://github.com/mismosmi/ai-pod/releases/download/v${AI_POD_VERSION}/host-tools-linux-${ARCH}" \
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
