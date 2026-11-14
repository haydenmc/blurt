FROM debian:trixie-slim

ARG USERNAME=dev
ARG USER_UID=1000
ARG USER_GID=1000

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8

# Base tooling + GPUI build/runtime deps + Mumble-related deps.
RUN apt-get update && apt-get install -y --no-install-recommends \
        # base
        ca-certificates curl git openssh-client sudo less procps jq ripgrep unzip \
        # toolchain
        build-essential pkg-config clang mold cmake \
        # GPUI (Wayland/X11, fonts, Vulkan via blade)
        libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev \
        libfontconfig-dev libzstd-dev libssl-dev \
        libvulkan1 mesa-vulkan-drivers vulkan-tools fonts-dejavu-core \
        # audio: ALSA dev headers (cpal), routed to host PipeWire/Pulse
        libasound2-dev libasound2-plugins pulseaudio-utils \
        # Mumble: protobuf (prost-build) and Opus codec
        protobuf-compiler libopus-dev \
    && rm -rf /var/lib/apt/lists/*

# Send ALSA's default device to the host's Pulse/PipeWire socket.
RUN printf 'pcm.!default { type pulse }\nctl.!default { type pulse }\n' > /etc/asound.conf

# Non-root user; UID matches host so rootless podman --userns=keep-id lines up.
RUN groupadd --gid ${USER_GID} ${USERNAME} \
    && useradd --uid ${USER_UID} --gid ${USER_GID} -m -s /bin/bash ${USERNAME} \
    && echo "${USERNAME} ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/${USERNAME} \
    && chmod 0440 /etc/sudoers.d/${USERNAME} \
    && mkdir -p /run/user/${USER_UID} \
    && chown ${USER_UID}:${USER_GID} /run/user/${USER_UID} \
    && chmod 0700 /run/user/${USER_UID}

USER ${USERNAME}
WORKDIR /home/${USERNAME}

ENV PATH=/home/${USERNAME}/.cargo/bin:/home/${USERNAME}/.local/bin:${PATH}

# Rust via rustup.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable \
            --component rustfmt,clippy,rust-analyzer,rust-src

# Claude Code via the native installer (not the devcontainer feature).
RUN curl -fsSL https://claude.ai/install.sh | bash

# Pre-create volume mount points so named volumes inherit user ownership.
RUN mkdir -p /home/${USERNAME}/.claude /home/${USERNAME}/.cargo/registry /home/${USERNAME}/.cargo/git
