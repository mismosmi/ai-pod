FROM {{BASE_IMAGE}}

{{INSTALL_PACKAGES}}
{{EXTRA_COMMANDS}}
ARG HOST_GATEWAY
ARG AI_POD_VERSION
# Fetch to a file first: `curl ... | bash` masks a failed download because the
# pipeline's exit status is bash's, not curl's, so a 404 (e.g. an outdated
# shared server that doesn't know this agent's install route) would silently
# produce an image without the agent. Splitting the steps lets `curl -f` fail
# the build loudly instead.
RUN curl -fsSL "http://${HOST_GATEWAY}:7822/install/{{AGENT}}.sh" -o /tmp/ai-pod-install.sh \
    && bash /tmp/ai-pod-install.sh \
    && rm -f /tmp/ai-pod-install.sh

WORKDIR /app

{{CREATE_USER}}

# System-level git identity (fallback when no host identity is provided)
RUN git config --system user.email "ai-pod@ai-pod" && \
    git config --system user.name "ai-pod"

USER ai-pod

ENV PATH="/home/ai-pod/.local/bin:${PATH}"
ENV EDITOR=vim

CMD ["{{AGENT}}"]
