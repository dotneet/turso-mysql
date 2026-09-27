FROM python:3.12.12-slim-bookworm@sha256:593bd06efe90efa80dc4eee3948be7c0fde4134606dd40d8dd8dbcade98e669c

# mysqlclient compiles against Debian's libmariadb when pip installs it.
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config default-libmysqlclient-dev gcc \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir --disable-pip-version-check -r requirements.txt
COPY . .
RUN chmod +x run.sh
CMD ["./run.sh"]
