"""Runs the E2E steps against app.db.engine.

Each step appends {"step", "ok", "error"} to $E2E_OUT/steps.jsonl and a failed
step never stops the run. Alembic commands run in-process through
alembic.command, which is what the `alembic` CLI calls.
"""

import json
import logging
import os
import shutil
import sys
import traceback
from decimal import Decimal
from pathlib import Path

from alembic import command as alembic_command
from alembic.config import Config
from alembic.runtime.migration import MigrationContext
from sqlalchemy import delete as sql_delete
from sqlalchemy import func, inspect, select, text
from sqlalchemy import update as sql_update
from sqlalchemy.dialects.mysql import insert as mysql_insert
from sqlalchemy.orm import Session, joinedload, selectinload

from app.db import engine
from app.models import Base, Post, Tag, User, post_tag

APP_DIR = Path(__file__).resolve().parent
STEPS_FILE = Path(os.environ.get("E2E_OUT", ".")) / "steps.jsonl"

logging.basicConfig(stream=sys.stdout, level=logging.INFO, format="%(name)s %(levelname)s %(message)s")
logging.getLogger("sqlalchemy.engine").setLevel(logging.INFO)


def main() -> None:
    for name, fn in STEPS:
        run_step(name, fn)


def run_step(name, fn) -> None:
    print(f"=== step {name}", flush=True)
    try:
        fn()
        record(name, True, "")
        print(f"=== step {name}: ok", flush=True)
    except BaseException:
        text_ = traceback.format_exc()
        print(text_, flush=True)
        record(name, False, text_[-2000:])
        print(f"=== step {name}: FAILED", flush=True)
        # A step that lost its connection must not hand it to the next one.
        engine.dispose()


def record(name: str, ok: bool, error: str) -> None:
    with STEPS_FILE.open("a") as f:
        f.write(json.dumps({"step": name, "ok": ok, "error": error}) + "\n")


def expect(actual, wanted, what: str) -> None:
    if actual != wanted:
        raise AssertionError(f"{what}: expected {wanted!r}, got {actual!r}")


def alembic_config() -> Config:
    return Config(str(APP_DIR / "alembic.ini"))


def connect() -> None:
    with engine.connect() as conn:
        row = conn.execute(text("SELECT VERSION(), @@character_set_client, DATABASE()")).one()
    print("server:", row)
    expect(row[1], "utf8mb4", "character_set_client")
    expect(row[2], os.environ.get("E2E_APP", "sqlalchemy"), "DATABASE()")


def migrate() -> None:
    alembic_command.upgrade(alembic_config(), "head")
    expect(current_revision(), "0001", "alembic revision")


def migrate_again() -> None:
    upgrades = RecordUpgrades()
    logging.getLogger("alembic.runtime.migration").addHandler(upgrades)
    try:
        alembic_command.upgrade(alembic_config(), "head")
    finally:
        logging.getLogger("alembic.runtime.migration").removeHandler(upgrades)
    expect(upgrades.messages, [], "migrations run by the second upgrade")
    expect(current_revision(), "0001", "alembic revision")


class RecordUpgrades(logging.Handler):
    def __init__(self) -> None:
        super().__init__()
        self.messages = []

    def emit(self, record: logging.LogRecord) -> None:
        if record.getMessage().startswith("Running upgrade"):
            self.messages.append(record.getMessage())


def current() -> None:
    alembic_command.current(alembic_config(), verbose=True)
    expect(current_revision(), "0001", "alembic revision")


def introspect() -> None:
    insp = inspect(engine)
    tables = sorted(insp.get_table_names())
    print("tables:", tables)
    expect(tables, ["alembic_version", "post_tag", "posts", "tags", "users"], "tables")

    columns = {c["name"]: c for c in insp.get_columns("users")}
    print("users columns:", columns)
    expect(list(columns), ["id", "email", "name", "balance", "is_active", "profile", "created_at", "updated_at"],
           "users columns")
    expect(columns["id"]["autoincrement"], True, "users.id autoincrement")
    expect(columns["email"]["type"].length, 191, "users.email length")
    expect((columns["balance"]["type"].precision, columns["balance"]["type"].scale), (10, 2), "users.balance")
    expect(columns["profile"]["nullable"], True, "users.profile nullable")
    expect(columns["email"]["nullable"], False, "users.email nullable")
    expect(type(columns["profile"]["type"]).__name__, "JSON", "users.profile type")

    expect(insp.get_pk_constraint("users")["constrained_columns"], ["id"], "users primary key")
    expect(insp.get_pk_constraint("post_tag")["constrained_columns"], ["post_id", "tag_id"], "post_tag primary key")

    uniques = insp.get_unique_constraints("users")
    print("users unique constraints:", uniques)
    expect([u["column_names"] for u in uniques], [["email"]], "users unique constraints")

    indexes = insp.get_indexes("posts")
    print("posts indexes:", indexes)
    expect([(i["name"], i["column_names"], i["unique"]) for i in indexes], [("ix_posts_user_id", ["user_id"], False)],
           "posts indexes")

    fks = insp.get_foreign_keys("posts")
    print("posts foreign keys:", fks)
    expect([(f["constrained_columns"], f["referred_table"], f["referred_columns"], f["options"].get("ondelete"))
            for f in fks], [(["user_id"], "users", ["id"], "CASCADE")], "posts foreign keys")
    pt_fks = sorted((f["referred_table"], f["options"].get("ondelete")) for f in insp.get_foreign_keys("post_tag"))
    expect(pt_fks, [("posts", "CASCADE"), ("tags", "CASCADE")], "post_tag foreign keys")


def alembic_check() -> None:
    alembic_command.check(alembic_config())


def insert() -> None:
    with Session(engine) as session:
        python, sql, news = Tag(name="python"), Tag(name="sql"), Tag(name="news")
        alice = User(email="alice@example.com", name="Alice", balance=Decimal("100.50"),
                     profile={"city": "Tokyo", "tags": ["a", "b"]})
        bob = User(email="bob@example.com", name="Bob", balance=Decimal("20.00"), profile={"city": "Osaka"})
        carol = User(email="carol@example.com", name="Carol", balance=Decimal("5.25"), is_active=False)
        alice.posts = [
            Post(title="Hello", body="first post", views=10, tags=[python, news]),
            Post(title="SQLAlchemy on MySQL", body="second", views=5, tags=[python, sql]),
        ]
        bob.posts = [
            Post(title="Bob's post", body="hi", views=1, tags=[news]),
            Post(title="Draft", body="unpublished"),
        ]
        session.add_all([alice, bob, carol])
        session.commit()
        ids = [alice.id, bob.id, carol.id]
        if None in ids or len(set(ids)) != 3:
            raise AssertionError(f"auto-increment ids not returned: {ids}")

    with Session(engine) as session:
        expect(session.scalar(select(func.count()).select_from(User)), 3, "users")
        expect(session.scalar(select(func.count()).select_from(Post)), 4, "posts")
        expect(session.scalar(select(func.count()).select_from(post_tag)), 5, "post_tag rows")
        fetched = session.scalars(select(User).where(User.email == "alice@example.com")).one()
        expect(fetched.balance, Decimal("100.50"), "alice balance")
        expect(fetched.profile, {"city": "Tokyo", "tags": ["a", "b"]}, "alice profile")
        expect(fetched.is_active, True, "alice is_active")
        if fetched.created_at is None:
            raise AssertionError("created_at not filled by the server default")


def relations() -> None:
    with Session(engine) as session:
        posts = session.scalars(
            select(Post).options(joinedload(Post.user), selectinload(Post.tags)).order_by(Post.title)
        ).unique().all()
        seen = [(p.title, p.user.name, sorted(t.name for t in p.tags)) for p in posts]
        print(seen)
        expect(seen, [
            ("Bob's post", "Bob", ["news"]),
            ("Draft", "Bob", []),
            ("Hello", "Alice", ["news", "python"]),
            ("SQLAlchemy on MySQL", "Alice", ["python", "sql"]),
        ], "posts with users and tags")

        users = session.scalars(select(User).options(selectinload(User.posts)).order_by(User.id)).all()
        expect([(u.name, len(u.posts)) for u in users], [("Alice", 2), ("Bob", 2), ("Carol", 0)], "users with posts")

        python_authors = session.scalars(
            select(User.name).join(User.posts).join(Post.tags).where(Tag.name == "python").distinct().order_by(User.name)
        ).all()
        expect(python_authors, ["Alice"], "authors of python posts (join)")

        news = session.scalars(select(Tag).where(Tag.name == "news")).one()
        expect(sorted(p.title for p in news.posts), ["Bob's post", "Hello"], "posts tagged news (lazy load)")


def update() -> None:
    with Session(engine) as session:
        result = session.execute(
            sql_update(User).where(User.email == "bob@example.com").values(balance=User.balance + 10)
        )
        expect(result.rowcount, 1, "rows updated")
        session.commit()
        bob = session.scalars(select(User).where(User.email == "bob@example.com")).one()
        expect(bob.balance, Decimal("30.00"), "bob balance")
        before = bob.updated_at
        bob.name = "Robert"
        session.commit()
        session.refresh(bob)
        expect(bob.name, "Robert", "bob name")
        if bob.updated_at < before:
            raise AssertionError("updated_at went backwards")
        result = session.execute(sql_update(Post).where(Post.views < 5).values(views=Post.views + 1))
        expect(result.rowcount, 2, "posts with few views")
        session.commit()


def delete() -> None:
    with Session(engine) as session:
        sql_tag = session.scalars(select(Tag).where(Tag.name == "sql")).one()
        temp = User(email="temp@example.com", name="Temp", posts=[Post(title="to be deleted", body="x", tags=[sql_tag])])
        session.add(temp)
        session.commit()
        session.delete(temp)
        session.commit()
        expect(session.scalar(select(func.count()).select_from(Post).where(Post.title == "to be deleted")), 0,
               "cascaded post")
        expect(session.scalar(select(func.count()).select_from(User).where(User.email == "temp@example.com")), 0,
               "deleted user")
        expect(session.scalar(select(func.count()).select_from(Tag).where(Tag.name == "sql")), 1, "tag kept")
        session.add(Tag(name="obsolete"))
        session.commit()
        result = session.execute(sql_delete(Tag).where(Tag.name == "obsolete"))
        expect(result.rowcount, 1, "deleted tags")
        session.commit()


def pagination() -> None:
    with Session(engine) as session:
        total = session.scalar(select(func.count()).select_from(Post))
        expect(total, 4, "total posts")
        page = session.scalars(select(Post.title).order_by(Post.title).limit(3).offset(3)).all()
        expect(page, ["SQLAlchemy on MySQL"], "page 2")
        window = session.scalars(select(Post.title).order_by(Post.title).limit(2).offset(1)).all()
        expect(window, ["Draft", "Hello"], "limit 2 offset 1")
        subquery_total = session.scalar(
            select(func.count()).select_from(select(Post.id).where(Post.views > 0).subquery())
        )
        expect(subquery_total, 4, "count over a subquery")


def aggregate() -> None:
    with Session(engine) as session:
        n, total, avg = session.execute(
            select(func.count(User.id), func.sum(User.balance), func.avg(User.balance))
        ).one()
        print(n, total, avg)
        expect((n, total, round(avg, 2)), (3, Decimal("135.75"), Decimal("45.25")), "count/sum/avg")

        busy = session.execute(
            select(User.name, func.count(Post.id), func.sum(Post.views))
            .join(User.posts)
            .group_by(User.id, User.name)
            .having(func.count(Post.id) >= 2)
            .order_by(User.id)
        ).all()
        expect([tuple(r) for r in busy], [("Alice", 2, Decimal("15")), ("Robert", 2, Decimal("3"))],
               "users with at least two posts (HAVING)")

        by_active = session.execute(
            select(User.is_active, func.count(), func.sum(User.balance)).group_by(User.is_active).order_by(User.is_active)
        ).all()
        expect([tuple(r) for r in by_active], [(False, 1, Decimal("5.25")), (True, 2, Decimal("130.50"))],
               "group by is_active")


def transaction_commit() -> None:
    with Session(engine) as session:
        with session.begin():
            dave = User(email="dave@example.com", name="Dave", balance=Decimal("1.00"))
            session.add(dave)
            session.flush()
            session.execute(sql_update(User).where(User.id == dave.id).values(balance=User.balance + 1))
    with Session(engine) as session:
        balance = session.scalar(select(User.balance).where(User.email == "dave@example.com"))
        expect(balance, Decimal("2.00"), "committed balance")


class Rollback(Exception):
    pass


def transaction_rollback() -> None:
    with Session(engine) as session:
        try:
            with session.begin():
                session.add(User(email="ghost@example.com", name="Ghost"))
                session.flush()
                raise Rollback()
        except Rollback:
            pass
        expect(user_exists(session, "ghost@example.com"), False, "rolled back user")

    with Session(engine) as session:
        with session.begin():
            session.add(User(email="outer@example.com", name="Outer"))
            try:
                with session.begin_nested():
                    session.add(User(email="inner@example.com", name="Inner"))
                    session.flush()
                    raise Rollback()
            except Rollback:
                pass
    with Session(engine) as session:
        expect(user_exists(session, "outer@example.com"), True, "outer user after savepoint rollback")
        expect(user_exists(session, "inner@example.com"), False, "inner user rolled back to savepoint")


def user_exists(session: Session, email: str) -> bool:
    return session.scalar(select(func.count()).select_from(User).where(User.email == email)) == 1


def json_path() -> None:
    with Session(engine) as session:
        tokyo = session.scalars(select(User.name).where(User.profile["city"].as_string() == "Tokyo")).all()
        expect(tokyo, ["Alice"], "profile.city = Tokyo")
        has_city = session.scalars(
            select(User.name).where(func.json_contains_path(User.profile, "one", "$.city")).order_by(User.name)
        ).all()
        expect(has_city, ["Alice", "Robert"], "profile has city")
        first_tag = session.scalars(select(User.profile[("tags", 0)].as_string()).where(User.name == "Alice")).one()
        expect(first_tag, "a", "profile.tags[0]")
        no_profile = session.scalars(select(User.name).where(User.profile.is_(None)).order_by(User.name)).all()
        expect(no_profile, ["Carol", "Dave", "Outer"], "profile is NULL")


def upsert() -> None:
    stmt = mysql_insert(User).values([
        {"email": "alice@example.com", "name": "Alice Updated", "balance": Decimal("200.00"), "is_active": True},
        {"email": "erin@example.com", "name": "Erin", "balance": Decimal("3.00"), "is_active": True},
    ])
    stmt = stmt.on_duplicate_key_update(name=stmt.inserted.name, balance=stmt.inserted.balance)
    with Session(engine) as session:
        session.execute(stmt)
        session.commit()
        alice = session.execute(select(User.name, User.balance).where(User.email == "alice@example.com")).one()
        expect(tuple(alice), ("Alice Updated", Decimal("200.00")), "alice after upsert")
        expect(user_exists(session, "erin@example.com"), True, "erin inserted by upsert")

        tag_stmt = mysql_insert(Tag).values(name="python")
        session.execute(tag_stmt.on_duplicate_key_update(name=tag_stmt.inserted.name))
        session.commit()
        expect(session.scalar(select(func.count()).select_from(Tag).where(Tag.name == "python")), 1, "python tag")


def core_text() -> None:
    with engine.begin() as conn:
        rows = conn.execute(
            text(
                "SELECT u.name, COUNT(p.id) AS n FROM users u LEFT JOIN posts p ON p.user_id = u.id "
                "WHERE u.email LIKE :pattern GROUP BY u.id, u.name ORDER BY u.id"
            ),
            {"pattern": "%@example.com"},
        ).all()
        print(rows)
        expect([tuple(r) for r in rows][:3], [("Alice Updated", 2), ("Robert", 2), ("Carol", 0)], "text() join")
        conn.execute(text("UPDATE users SET is_active = :active WHERE email = :email"),
                     {"active": False, "email": "erin@example.com"})
        erin = conn.execute(text("SELECT is_active FROM users WHERE email = :email"),
                            {"email": "erin@example.com"}).scalar_one()
        expect(erin, 0, "erin is_active via text()")
        many = conn.execute(text("INSERT INTO tags (name) VALUES (:name)"), [{"name": "t1"}, {"name": "t2"}])
        expect(many.rowcount, 2, "executemany rowcount")


def alter_migration() -> None:
    source = APP_DIR / "later_versions" / "0002_post_slug_content.py"
    # The second revision ships the way a later deploy does: the file appears
    # in migrations/versions only now, so the first `upgrade head` stops at 0001.
    shutil.copy(source, APP_DIR / "migrations" / "versions" / source.name)
    alembic_command.upgrade(alembic_config(), "0002")
    expect(current_revision(), "0002", "alembic revision")
    insp = inspect(engine)
    columns = {c["name"]: c for c in insp.get_columns("posts")}
    print("posts columns:", columns)
    for name in ("slug", "content"):
        if name not in columns:
            raise AssertionError(f"posts.{name} missing after 0002: {sorted(columns)}")
    if "body" in columns:
        raise AssertionError("posts.body still present after rename")
    expect(columns["slug"]["nullable"], True, "posts.slug nullable")
    expect(type(columns["views"]["type"]).__name__, "BIGINT", "posts.views type")
    indexes = {i["name"]: i["column_names"] for i in insp.get_indexes("posts")}
    expect(indexes.get("ix_posts_slug"), ["slug"], "ix_posts_slug")
    with engine.connect() as conn:
        expect(conn.execute(text("SELECT COUNT(*) FROM posts WHERE content IS NOT NULL")).scalar_one(), 4,
               "posts keep their body after the rename")


def rollback_migration() -> None:
    alembic_command.downgrade(alembic_config(), "-1")
    expect(current_revision(), "0001", "alembic revision")
    insp = inspect(engine)
    columns = {c["name"]: c for c in insp.get_columns("posts")}
    expect(sorted(columns), ["body", "id", "published_at", "title", "user_id", "views"], "posts columns")
    expect(type(columns["views"]["type"]).__name__, "INTEGER", "posts.views type")
    expect([i["name"] for i in insp.get_indexes("posts")], ["ix_posts_user_id"], "posts indexes")
    with Session(engine) as session:
        expect(session.scalar(select(func.count()).select_from(Post).where(Post.body == "first post")), 1,
               "body readable through the ORM again")


def drop_all() -> None:
    Base.metadata.drop_all(engine)
    expect(inspect(engine).get_table_names(), ["alembic_version"], "tables after drop_all")


def current_revision():
    with engine.connect() as conn:
        return MigrationContext.configure(conn).get_current_revision()


STEPS = [
    ("connect", connect),
    ("migrate", migrate),
    ("migrate-again", migrate_again),
    ("current", current),
    ("introspect", introspect),
    ("alembic-check", alembic_check),
    ("insert", insert),
    ("relations", relations),
    ("update", update),
    ("delete", delete),
    ("pagination", pagination),
    ("aggregate", aggregate),
    ("transaction-commit", transaction_commit),
    ("transaction-rollback", transaction_rollback),
    ("json", json_path),
    ("upsert", upsert),
    ("core-text", core_text),
    ("alter-migration", alter_migration),
    ("rollback-migration", rollback_migration),
    ("drop-all", drop_all),
]

if __name__ == "__main__":
    main()
