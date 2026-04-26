FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    gettext-base \
    gnupg \
    jq \
    python3 \
    supervisor \
    openssh-client \
    git \
    unzip \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://bun.sh/install | bash \
    && mv /root/.bun/bin/bun /usr/local/bin/bun \
    && ln -s /usr/local/bin/bun /usr/local/bin/bunx
ENV BUN_INSTALL_BIN=/usr/local/bin

COPY containers/apps/scripts/github-app-token.sh /usr/local/bin/github-app-token
RUN chmod +x /usr/local/bin/github-app-token

COPY containers/apps/scripts/ssh-agent-add-keys.sh /usr/local/bin/ssh-agent-add-keys.sh
RUN chmod +x /usr/local/bin/ssh-agent-add-keys.sh

RUN ssh-keyscan github.com >> /etc/ssh/ssh_known_hosts 2>/dev/null

RUN mkdir -p /etc/supervisor/projects.d

COPY containers/apps/supervisor/supervisord.conf /etc/supervisor/supervisord.conf
COPY containers/apps/supervisor/projects.conf /etc/supervisor/projects.conf
COPY containers/apps/supervisor/conf.d/ /etc/supervisor/conf.d/

COPY containers/apps/docker-entrypoint.d/ /docker-entrypoint.d/
RUN chmod +x /docker-entrypoint.d/*.sh

COPY containers/apps/scripts/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

COPY containers/apps/scripts/healthcheck.sh /usr/local/bin/healthcheck
RUN chmod +x /usr/local/bin/healthcheck

RUN mkdir -p /var/log/supervisor

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=3 \
    CMD /usr/local/bin/healthcheck

ENTRYPOINT ["/entrypoint.sh"]
