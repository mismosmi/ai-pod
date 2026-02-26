FROM ubuntu:latest

RUN apt-get update && apt-get install -y curl git

WORKDIR /app

RUN useradd -ms /bin/bash claude
RUN chown -R claude /app

# System-level git identity
RUN git config --system user.email "claude@ai-pod" && \
    git config --system user.name "claude"

USER claude

ENV PATH="/home/claude/.local/bin:${PATH}"

# Install claude as the claude user so all symlinks/node modules land in ~/.local/
RUN curl -fsSL https://claude.ai/install.sh | bash

CMD ["claude"]
