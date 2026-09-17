# syntax=docker/dockerfile:1
# Builds from the context .github/scripts/image-context.sh lays out, not from the repository root.
FROM scratch
ARG TARGETPLATFORM
# Workflow artifacts drop the execute bit.
COPY --chmod=0755 ${TARGETPLATFORM}/hawse /usr/local/bin/hawse
# A chowned COPY also chowns the parent directories it creates, so create them as root first.
COPY state /var/lib
# A named volume mounted here starts with this ownership.
COPY --chown=65532:65532 state /var/lib/hawse
# A non-root hawse reads config from $XDG_CONFIG_HOME/hawse or else /etc/hawse, and without a
# config writes keys to the first.
ENV XDG_CONFIG_HOME=/var/lib
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/hawse"]
