# A stock Laravel 12 skeleton with pdo_mysql, plus this app's migrations,
# models and the e2e:step artisan command copied over it.
FROM composer:2.10.3@sha256:9715c7f69044da2a212a5fbde29ee7da24e364d426560ae6367b060236f847d7 AS composer

# Target of its own so composer.lock can be regenerated from the same skeleton.
FROM php:8.3.35-cli-bookworm@sha256:4687aec76c4a895b68b91bcd5e48f1ba7a3dea120f6de50bba7e1a93af5372dd AS skeleton
# intl: db:show and db:table format table sizes with Illuminate\Support\Number.
RUN apt-get update \
    && apt-get install -y --no-install-recommends libicu-dev unzip \
    && docker-php-ext-install pdo_mysql intl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=composer /usr/bin/composer /usr/bin/composer
ENV COMPOSER_ALLOW_SUPERUSER=1
RUN composer create-project --no-install --no-scripts --no-interaction \
    laravel/laravel=12.12.2 /app
WORKDIR /app

FROM skeleton
COPY composer.lock /app/composer.lock
RUN composer install --no-interaction --no-progress \
    && cp .env.example .env \
    && php artisan key:generate --no-interaction
COPY app/ /app/app/
COPY database/ /app/database/
COPY run.sh /app/run.sh
RUN chmod +x /app/run.sh
