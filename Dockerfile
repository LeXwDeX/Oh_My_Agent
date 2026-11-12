FROM python:3.13-alpine

WORKDIR /app
COPY server.py index.html classic.html office.html city.html ./
COPY avatars ./avatars

ENV PYTHONUNBUFFERED=1 \
    PYTHONDONTWRITEBYTECODE=1

# hub 模式不读宿主机文件，用非 root 运行；local 模式（docker-compose.local.yml）需要读挂载的会话文件，会覆盖为 root
RUN adduser -D -H -u 10001 deck
USER deck

EXPOSE 7788
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD wget -qO- "http://127.0.0.1:${DECK_PORT:-7788}/healthz" >/dev/null || exit 1

CMD ["python", "server.py"]
