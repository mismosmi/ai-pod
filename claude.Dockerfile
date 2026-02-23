FROM ubuntu:latest

RUN apt-get update && apt-get install -y curl git

WORKDIR /app

RUN useradd -ms /bin/bash claude
RUN chown -R claude /app

# Install claude as root then move to system-wide location
RUN curl -fsSL https://claude.ai/install.sh | bash && \
    mv /root/.local/bin/claude /usr/local/bin/claude

# System-level git identity
RUN git config --system user.email "claude@ai-pod" && \
    git config --system user.name "claude"

USER claude

CMD ["claude"]
