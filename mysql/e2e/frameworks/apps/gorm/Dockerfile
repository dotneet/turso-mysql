FROM golang:1.25.2-bookworm@sha256:42d8e9dea06f23d0bfc908826455213ee7f3ed48c43e287a422064220c501be9 AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY *.go ./
RUN CGO_ENABLED=0 GOFLAGS=-trimpath go build -o /out/gorm-e2e .

FROM debian:bookworm-20260918-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
WORKDIR /app
COPY --from=build /out/gorm-e2e /app/gorm-e2e
COPY run.sh /app/run.sh
RUN chmod +x /app/run.sh
