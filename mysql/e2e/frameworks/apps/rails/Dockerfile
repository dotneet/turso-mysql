FROM ruby:3.4.11-bookworm@sha256:246b2dc3f6e40bba3af18503c22997a34dbb27c9f97e198dde6dd727895115c5
RUN apt-get update \
  && apt-get install -y --no-install-recommends build-essential default-libmysqlclient-dev \
  && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Gemfile Gemfile.lock ./
RUN bundle config set --local frozen true && bundle install --jobs 4
COPY . .
RUN chmod +x run.sh
