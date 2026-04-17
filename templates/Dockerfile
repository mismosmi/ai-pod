FROM {{BASE_IMAGE}}

{{INSTALL_PACKAGES}}

ARG HOST_GATEWAY
RUN curl -fsSL "http://${HOST_GATEWAY}:7822/host-tools" \
      -o /usr/local/bin/host-tools && chmod +x /usr/local/bin/host-tools

ARG AI_POD_VERSION
RUN host-tools install {{AGENT}}

WORKDIR /app

{{CREATE_USER}}

# System-level git identity (fallback when no host identity is provided)
RUN git config --system user.email "{{AGENT}}@ai-pod" && \
    git config --system user.name "{{AGENT}}"

USER {{AGENT}}

ENV PATH="/home/{{AGENT}}/.local/bin:${PATH}"
ENV EDITOR=vim

CMD ["{{AGENT}}"]
