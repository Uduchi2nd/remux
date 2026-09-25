FROM python:3.12-slim
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir "fastapi==0.115.*" "uvicorn[standard]==0.30.*" "numpy==2.*"
WORKDIR /app
COPY dubmux.py server.py /app/
ENV VNEXT_DATA=/data VNEXT_WORKERS=64 VNEXT_WINDOW=48 VNEXT_JOBS=2
EXPOSE 19000
CMD ["uvicorn", "server:app", "--host", "0.0.0.0", "--port", "19000", "--workers", "1", "--timeout-keep-alive", "75"]
