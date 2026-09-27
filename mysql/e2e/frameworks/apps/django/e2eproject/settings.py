import os
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent.parent

SECRET_KEY = "e2e-only-not-a-secret"
# Django logs SQL only when DEBUG is on.
DEBUG = True
ALLOWED_HOSTS = []

INSTALLED_APPS = [
    "django.contrib.auth",
    "django.contrib.contenttypes",
    "django.contrib.sessions",
    "blog",
]

DATABASES = {
    "default": {
        "ENGINE": "django.db.backends.mysql",
        "NAME": os.environ.get("E2E_APP", "django"),
        "USER": os.environ.get("E2E_USER", "e2e"),
        "PASSWORD": os.environ.get("E2E_PASSWORD", ""),
        "HOST": os.environ.get("E2E_HOST", "127.0.0.1"),
        "PORT": os.environ.get("E2E_PORT", "3306"),
        "OPTIONS": {
            "ssl": {"ca": os.environ.get("E2E_CA", "/e2e/tls/ca.pem")},
            # Built against libmariadb, mysqlclient checks the server
            # certificate only from VERIFY_CA up; "ssl" alone means REQUIRED.
            "ssl_mode": "VERIFY_IDENTITY",
            "charset": "utf8mb4",
        },
    }
}

DEFAULT_AUTO_FIELD = "django.db.models.BigAutoField"
USE_TZ = True
TIME_ZONE = "UTC"

LOGGING = {
    "version": 1,
    "disable_existing_loggers": False,
    "handlers": {
        "stdout": {"class": "logging.StreamHandler", "stream": "ext://sys.stdout"},
    },
    "loggers": {
        "django.db.backends": {"handlers": ["stdout"], "level": "DEBUG", "propagate": False},
    },
}
