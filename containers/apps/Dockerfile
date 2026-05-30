FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    gettext-base \
    gnupg \
    jq \
    nginx \
    pkg-config \
    python3 \
    openssh-client \
    openssh-server \
    git \
    locales \
    unzip \
    sudo \
    && sed -i '/en_US.UTF-8/s/^# //g' /etc/locale.gen \
    && locale-gen en_US.UTF-8 \
    && rm -rf /var/lib/apt/lists/*

ENV LANG=en_US.UTF-8
ENV LC_ALL=en_US.UTF-8

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

RUN usermod -l gem -d /home/gem -m ubuntu && groupmod -n gem ubuntu \
    && passwd -u gem 2>/dev/null || usermod -p '*' gem \
    && echo "gem ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/gem \
    && chmod 440 /etc/sudoers.d/gem

COPY containers/apps/sshd_config /etc/ssh/sshd_config
RUN mkdir -p /run/sshd /etc/ssh/authorized_keys
COPY containers/apps/ssh/sandbox-to-apps.pub /etc/ssh/authorized_keys/gem
RUN chmod 644 /etc/ssh/authorized_keys/gem

COPY containers/apps/nginx.conf /etc/nginx/nginx.conf
RUN mkdir -p /etc/nginx/conf.d

COPY containers/apps/launchy/config.json /etc/launchy/config.json
COPY containers/apps/launchy/built-in/ /etc/launchy/built-in/
RUN mkdir -p /etc/launchy/apps.d /var/log/launchy /run

COPY containers/apps/docker-entrypoint.d/ /docker-entrypoint.d/
RUN chmod +x /docker-entrypoint.d/*.sh

COPY containers/apps/scripts/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

COPY containers/apps/scripts/healthcheck.sh /usr/local/bin/healthcheck
RUN chmod +x /usr/local/bin/healthcheck

COPY target/launchy /sbin/launchy
RUN chmod +x /sbin/launchy

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=3 \
    CMD /usr/local/bin/healthcheck

ENTRYPOINT ["/entrypoint.sh"]
