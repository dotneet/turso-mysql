FROM mcr.microsoft.com/dotnet/sdk:8.0.425-bookworm-slim@sha256:78235e09001f52b6592c458ac010775ebac6725422e80cd0c1650590f67b2743

ENV DOTNET_CLI_TELEMETRY_OPTOUT=1 \
    DOTNET_NOLOGO=1 \
    DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1

WORKDIR /app
COPY .config ./.config
COPY E2e.csproj packages.lock.json ./
RUN dotnet tool restore && dotnet restore --locked-mode
COPY . .
# `dotnet ef` runs with --no-build against this Debug build.
RUN dotnet build --no-restore && chmod +x run.sh
