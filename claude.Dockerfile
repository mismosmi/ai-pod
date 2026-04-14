FROM ubuntu:latest

RUN apt-get update && apt-get install -y curl git vim

WORKDIR /app

# System-level git identity
RUN git config --system user.email "claude@ai-pod" && \
    git config --system user.name "claude"

USER ubuntu

ENV PATH="/home/claude/.local/bin:${PATH}"
ENV EDITOR=vim
