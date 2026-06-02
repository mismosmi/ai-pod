FROM {{BASE_IMAGE}}

{{INSTALL_PACKAGES}}
{{EXTRA_COMMANDS}}
ARG HOST_GATEWAY
ARG AI_POD_VERSION
RUN curl -fsSL "http://${HOST_GATEWAY}:7822/install/{{AGENT}}.sh" | bash

# Clipboard bridge: fake xclip/wl-paste shims so Ctrl+V image paste works.
# Referencing AI_POD_VERSION busts this layer's cache on a version bump so the
# shims refresh on rebuild.
RUN echo "ai-pod ${AI_POD_VERSION}" && \
    curl -fsSL "http://${HOST_GATEWAY}:7822/install/clipboard.sh" | bash

WORKDIR /app

{{CREATE_USER}}

# System-level git identity (fallback when no host identity is provided)
RUN git config --system user.email "ai-pod@ai-pod" && \
    git config --system user.name "ai-pod"

USER ai-pod

ENV PATH="/home/ai-pod/.local/bin:${PATH}"
ENV EDITOR=vim

CMD ["{{AGENT}}"]
