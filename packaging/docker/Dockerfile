# Unofficial UU Remote Linux host in a container, for NAS boxes (Synology DSM, etc.) without a desktop.
# Headless mode: file transfer, terminal and port mapping; no remote desktop. Uses the published release binary.
FROM ubuntu:24.04
ARG VERSION=v0.2.20
LABEL org.opencontainers.image.source="https://github.com/VallierDev/uu-remote-linux" \
      org.opencontainers.image.description="Unofficial UU Remote Linux host, headless (file transfer, terminal, port mapping)" \
      org.opencontainers.image.licenses="GPL-3.0"
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates curl bash libx11-6 libxext6 libxtst6 libxfixes3 libgtk-3-0 \
 && cd /tmp \
 && base="https://github.com/VallierDev/uu-remote-linux/releases/download/${VERSION}/uu-remote-linux-${VERSION}-x86_64-linux.tar.gz" \
 && curl -fsSLO "$base" && curl -fsSLO "$base.sha256" && sha256sum -c "uu-remote-linux-${VERSION}-x86_64-linux.tar.gz.sha256" \
 && tar xzf "uu-remote-linux-${VERSION}-x86_64-linux.tar.gz" \
 && install -m755 "$(find /tmp -name uuremote-server -type f | head -1)" /usr/local/bin/uuremote-server \
 && rm -rf /tmp/* /var/lib/apt/lists/*
# Credentials live in /config (keep it private); /data is what file transfer and the terminal see as home.
ENV UU_CONFIG=/config/config.json UU_DESKTOP_BACKEND=none UU_LOGIN_WINDOW=0 UU_AUDIO=0 RUST_LOG=info HOME=/data
VOLUME ["/config", "/data"]
WORKDIR /data
ENTRYPOINT ["uuremote-server"]
