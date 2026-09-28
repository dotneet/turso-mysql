# Gitea's own integration suite (tests/integration), compiled once and run
# against the target server through the harness proxy. The source tree stays
# in the image: the suite reads its fixtures, templates and test repositories
# from it.
FROM golang:1.26-bookworm AS build
ARG GITEA_REF=v1.27.3
RUN apt-get update && apt-get install -y --no-install-recommends git git-lfs && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch "${GITEA_REF}" https://github.com/go-gitea/gitea.git /gitea
WORKDIR /gitea
RUN go mod download
RUN CGO_ENABLED=0 go test -c -o /gitea/integration.test ./tests/integration
# The tests that drive Gitea Actions through a mock runner, listed for run.sh
# to leave out.
RUN cd tests/integration \
  && { ls actions_*.go api_actions_*.go; grep -l mockRunner *.go; } | sort -u \
  | xargs grep -ho '^func Test[A-Za-z0-9_]*' | sed 's/^func //' | sort -u >/gitea/runner-tests.txt \
  && wc -l /gitea/runner-tests.txt

FROM golang:1.26-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends git git-lfs python3 && rm -rf /var/lib/apt/lists/*
# Gitea refuses to run as root, and its test setup clears the variable that
# would allow it, so the suite runs as a user of its own.
RUN useradd --create-home --uid 1000 gitea
COPY --from=build --chown=gitea:gitea /gitea /gitea
COPY run.sh steps.py /app/
RUN chmod +x /app/run.sh
