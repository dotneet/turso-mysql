import os

from sqlalchemy import URL, create_engine

url = URL.create(
    "mysql+pymysql",
    username=os.environ.get("E2E_USER", "e2e"),
    password=os.environ.get("E2E_PASSWORD", ""),
    host=os.environ.get("E2E_HOST", "127.0.0.1"),
    port=int(os.environ.get("E2E_PORT", "3306")),
    database=os.environ.get("E2E_APP", "sqlalchemy"),
    query={"charset": "utf8mb4"},
)

engine = create_engine(
    url,
    connect_args={"ssl": {"ca": os.environ.get("E2E_CA", "/e2e/tls/ca.pem")}},
)
