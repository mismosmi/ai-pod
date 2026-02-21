FROM rust:latest

WORKDIR /app

RUN useradd -ms /bin/bash claude
RUN chown -R claude /app
USER claude


ENV PATH="/home/claude/.local/bin:$PATH"
RUN curl -fsSL https://claude.ai/install.sh | bash


CMD ["claude"]
