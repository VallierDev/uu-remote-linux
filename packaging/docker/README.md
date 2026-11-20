# Docker（群晖 / NAS，无桌面）

本目录是 [`ghcr.io/vallierdev/uu-remote-linux`](https://github.com/VallierDev/uu-remote-linux/pkgs/container/uu-remote-linux) 镜像的构建文件。
**部署和使用步骤见 [docs/deploy-nas-docker.md](../docs/deploy-nas-docker.md)。**

This directory builds the `ghcr.io/vallierdev/uu-remote-linux` image; see
[docs/deploy-nas-docker.md](../docs/deploy-nas-docker.md#english) for deployment.

自行构建 / build locally: `docker compose build`（`Dockerfile` 从 GitHub 发布包安装 `uuremote-server`，可用 `--build-arg VERSION=vX.Y.Z` 指定版本）。
