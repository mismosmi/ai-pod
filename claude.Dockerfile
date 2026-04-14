FROM ubuntu:latest

RUN apt-get update && apt-get install -y curl git vim

WORKDIR /app

RUN useradd -u 1000 -ms /bin/bash claude && chown -R claude /app

# System-level git identity
RUN git config --system user.email "claude@ai-pod" && \
    git config --system user.name "claude"

USER claude

ENV PATH="/home/claude/.local/bin:${PATH}"
ENV EDITOR=vim
