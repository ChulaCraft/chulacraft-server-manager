# Real Apache 2.4 (Ubuntu 22.04: 2.4.52) proxying to the manager over a unix socket.
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        apache2 curl openssl libsystemd0 util-linux ca-certificates \
    && rm -rf /var/lib/apt/lists/*
