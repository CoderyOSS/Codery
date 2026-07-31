Recommended approach

For a development container, the established pattern is:

1. Create a regular user.
2. Install sudo.
3. Add one small file under /etc/sudoers.d/.
4. Validate it with visudo.
5. Run the container as that user.

This keeps the normal shell and application processes unprivileged while allowing explicit elevation through sudo. Docker recommends using USER when software does not need to run continuously as root. 

Minimal Ubuntu Dockerfile

FROM ubuntu:24.04
ARG USERNAME=dev
ARG USER_UID=1000
ARG USER_GID=$USER_UID
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        sudo \
    && groupadd --gid "$USER_GID" "$USERNAME" \
    && useradd \
        --uid "$USER_UID" \
        --gid "$USER_GID" \
        --create-home \
        --shell /bin/bash \
        "$USERNAME" \
    && printf '%s ALL=(ALL:ALL) NOPASSWD: ALL\n' "$USERNAME" \
        > "/etc/sudoers.d/$USERNAME" \
    && chmod 0440 "/etc/sudoers.d/$USERNAME" \
    && visudo --check --file="/etc/sudoers.d/$USERNAME" \
    && rm -rf /var/lib/apt/lists/*
USER $USERNAME
WORKDIR /home/$USERNAME

Build and test it:

docker build -t sudo-dev .
docker run --rm -it sudo-dev

Inside the container:

whoami
# dev
sudo whoami
# root

Applied to your existing Ubuntu image

You already install sudo, so you only need to add the user configuration:

ARG USERNAME=dev
ARG USER_UID=1000
ARG USER_GID=$USER_UID
RUN groupadd --gid "$USER_GID" "$USERNAME" \
    && useradd \
        --uid "$USER_UID" \
        --gid "$USER_GID" \
        --create-home \
        --shell /bin/bash \
        "$USERNAME" \
    && printf '%s ALL=(ALL:ALL) NOPASSWD: ALL\n' "$USERNAME" \
        > "/etc/sudoers.d/$USERNAME" \
    && chmod 0440 "/etc/sudoers.d/$USERNAME" \
    && visudo --check --file="/etc/sudoers.d/$USERNAME"
USER $USERNAME
WORKDIR /home/$USERNAME

Put that after package installation and before your ENTRYPOINT or CMD.

Why /etc/sudoers.d/ instead of editing /etc/sudoers

The sudo policy is controlled by /etc/sudoers and its included configuration files. Using a dedicated file:

* avoids modifying the package-managed main file;
* isolates your container-specific rule;
* is easy to remove or audit;
* allows validation during the image build.

The 0440 permission is important because sudo rejects improperly protected configuration in normal configurations. visudo --check catches syntax errors at build time rather than leaving you with an image whose sudo access is broken. 

Should it be passwordless?

For an interactive, disposable development container, usually yes:

dev ALL=(ALL:ALL) NOPASSWD: ALL

Giving the user a password generally adds ceremony without adding much protection inside a single-user development container. Anyone who controls that container session or can execute as dev can become root.

For a shared or remotely exposed container, restrict commands instead:

dev ALL=(root) NOPASSWD: /usr/bin/apt-get, /usr/bin/apt

Be careful: package managers and many seemingly limited commands can still provide effective arbitrary root access. A truly constrained sudo policy requires auditing every permitted executable and its arguments.

A reusable helper script

For several images, move the setup into a script:

#!/usr/bin/env bash
set -Eeuo pipefail
username="${1:-dev}"
uid="${2:-1000}"
gid="${3:-$uid}"
groupadd --gid "$gid" "$username"
useradd \
  --uid "$uid" \
  --gid "$gid" \
  --create-home \
  --shell /bin/bash \
  "$username"
printf '%s ALL=(ALL:ALL) NOPASSWD: ALL\n' "$username" \
  > "/etc/sudoers.d/$username"
chmod 0440 "/etc/sudoers.d/$username"
visudo --check --file="/etc/sudoers.d/$username"

Dockerfile:

COPY create-dev-user /usr/local/bin/create-dev-user
RUN chmod +x /usr/local/bin/create-dev-user \
    && /usr/local/bin/create-dev-user dev 1000 1000 \
    && rm /usr/local/bin/create-dev-user
USER dev

For one image, the inline Dockerfile is cleaner. The helper becomes worthwhile only when multiple images share the setup.

UID and bind-mount permissions

UID 1000 is a convenient default, but Linux bind-mounted files are governed by numeric UID/GID, not the username displayed inside the container.

Build with your host identity when needed:

docker build \
  --build-arg USER_UID="$(id -u)" \
  --build-arg USER_GID="$(id -g)" \
  -t sudo-dev .

On Docker Desktop for macOS, filesystem sharing is virtualized, so UID matching is often less visible than on a native Linux Docker host. It is still sensible to keep the build arguments.

Important security boundary

sudo grants root inside the container. It does not inherently grant unrestricted root access to the host. Container isolation, capabilities, seccomp, AppArmor or SELinux, namespaces, mounts, and daemon configuration still apply. Docker can additionally remap container root to an unprivileged host UID through user namespaces. 

However, container root can effectively become host root when you provide dangerous access, especially:

-v /var/run/docker.sock:/var/run/docker.sock

or:

--privileged

or broad writable host mounts such as:

-v /:/host

Therefore, passwordless sudo is reasonable for a contained dev environment, but it should not be combined casually with privileged mode, the host Docker socket, host devices, or sensitive writable mounts.

Production guidance

For production images, do not normally install sudo at all:

FROM ubuntu:24.04
RUN groupadd --system app \
    && useradd \
        --system \
        --gid app \
        --create-home \
        app
COPY --chown=app:app . /app
USER app
WORKDIR /app
CMD ["./start"]

Administrative work belongs in image build steps or orchestration, not interactive runtime elevation. Running the final process as a non-root user reduces the impact of an application compromise and follows Docker’s published hardening guidance. 

Bottom line

For your remote development image, this is the concise version I would use:

ARG USERNAME=dev
ARG USER_UID=1000
ARG USER_GID=$USER_UID
RUN groupadd -g "$USER_GID" "$USERNAME" \
    && useradd -m -s /bin/bash -u "$USER_UID" -g "$USER_GID" "$USERNAME" \
    && echo "$USERNAME ALL=(ALL:ALL) NOPASSWD: ALL" \
        > "/etc/sudoers.d/$USERNAME" \
    && chmod 0440 "/etc/sudoers.d/$USERNAME" \
    && visudo -cf "/etc/sudoers.d/$USERNAME"
USER $USERNAME
WORKDIR /home/$USERNAME

It is standard Linux machinery, requires no entrypoint tricks, performs validation during the build, and leaves the container running non-root by default.

