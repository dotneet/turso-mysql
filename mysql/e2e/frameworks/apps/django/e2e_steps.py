"""Runs the E2E steps against the database in DATABASES["default"].

Each step appends {"step", "ok", "error"} to $E2E_OUT/steps.jsonl and a failed
step never stops the run. Management commands run in-process through
call_command, which is what `manage.py <command>` does.
"""

import importlib
import io
import json
import os
import shutil
import sys
import traceback
from decimal import Decimal
from pathlib import Path

os.environ.setdefault("DJANGO_SETTINGS_MODULE", "e2eproject.settings")

import django  # noqa: E402

django.setup()

from django.core.management import call_command  # noqa: E402
from django.core.paginator import Paginator  # noqa: E402
from django.db import connection, connections, transaction  # noqa: E402
from django.db.models import Avg, Count, F, Sum  # noqa: E402

from blog.models import Post, Tag, User  # noqa: E402

APP_DIR = Path(__file__).resolve().parent
STEPS_FILE = Path(os.environ.get("E2E_OUT", ".")) / "steps.jsonl"


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
        text = traceback.format_exc()
        print(text, flush=True)
        record(name, False, text[-2000:])
        print(f"=== step {name}: FAILED", flush=True)
        # A step that failed mid-transaction or lost its connection must not
        # poison the next one.
        try:
            connections.close_all()
        except BaseException:
            traceback.print_exc(file=sys.stdout)


def record(name: str, ok: bool, error: str) -> None:
    with STEPS_FILE.open("a") as f:
        f.write(json.dumps({"step": name, "ok": ok, "error": error}) + "\n")


def command(*args, **kwargs) -> str:
    out = io.StringIO()
    call_command(*args, stdout=out, stderr=out, **kwargs)
    text = out.getvalue()
    print(text, flush=True)
    return text


def expect(actual, wanted, what: str) -> None:
    if actual != wanted:
        raise AssertionError(f"{what}: expected {wanted!r}, got {actual!r}")


def connect_default_charset() -> None:
    # Django's MySQL backend asks for charset "utf8" (utf8mb3) unless OPTIONS
    # overrides it; this is what a project without that override gets.
    default = connections["default"]
    settings_dict = dict(default.settings_dict)
    settings_dict["OPTIONS"] = {k: v for k, v in settings_dict["OPTIONS"].items() if k != "charset"}
    other = type(default)(settings_dict, alias="default_charset")
    try:
        with other.cursor() as cursor:
            cursor.execute("SELECT @@character_set_client, @@character_set_connection")
            print("default charset session:", cursor.fetchone())
    finally:
        other.close()


def connect() -> None:
    with connection.cursor() as cursor:
        cursor.execute("SELECT VERSION(), @@character_set_client, DATABASE()")
        row = cursor.fetchone()
    print("server:", row, "mysql_version:", connection.mysql_version)
    expect(row[1], "utf8mb4", "character_set_client")
    expect(row[2], os.environ.get("E2E_APP", "django"), "DATABASE()")


def migrate() -> None:
    command("migrate", interactive=False)
    expect(latest_blog_migration(), "0001_initial", "applied blog migration")


def migrate_again() -> None:
    out = command("migrate", interactive=False)
    if "No migrations to apply." not in out:
        raise AssertionError("second migrate was not a no-op")


def makemigrations_check() -> None:
    command("makemigrations", check=True, dry_run=True)


def check() -> None:
    command("check", databases=["default"])


def showmigrations() -> None:
    out = command("showmigrations")
    for line in ("[X] 0001_initial", "[X] 0012_alter_user_first_name_max_length"):
        if line not in out:
            raise AssertionError(f"showmigrations lacks {line!r}")


def sqlmigrate() -> None:
    out = command("sqlmigrate", "blog", "0001")
    for fragment in ("CREATE TABLE `users`", "CREATE TABLE `post_tag`", "FOREIGN KEY (`user_id`)"):
        if fragment not in out:
            raise AssertionError(f"sqlmigrate output lacks {fragment!r}")


def introspect() -> None:
    out = command("inspectdb", "users", "posts", "tags", "post_tag")
    for fragment in (
        "class Users(models.Model):",
        "email = models.CharField(unique=True, max_length=191)",
        "balance = models.DecimalField(max_digits=10, decimal_places=2)",
        "profile = models.JSONField(blank=True, null=True)",
        "user = models.ForeignKey(Users, models.DO_NOTHING)",
        "unique_together = (('post', 'tag'),)",
    ):
        if fragment not in out:
            raise AssertionError(f"inspectdb output lacks {fragment!r}")

    intro = connection.introspection
    with connection.cursor() as cursor:
        tables = {t.name: t.type for t in intro.get_table_list(cursor)}
        print("tables:", tables)
        for name in ("users", "posts", "tags", "post_tag", "auth_user", "django_migrations"):
            expect(tables.get(name), "t", f"table {name}")

        columns = {c.name: c for c in intro.get_table_description(cursor, "users")}
        print("users columns:", columns)
        expect(sorted(columns), sorted(
            ["id", "email", "name", "balance", "is_active", "profile", "created_at", "updated_at"]
        ), "users columns")
        expect(bool(columns["profile"].null_ok), True, "users.profile nullable")
        expect(bool(columns["email"].null_ok), False, "users.email nullable")
        expect((columns["balance"].precision, columns["balance"].scale), (10, 2), "users.balance precision")
        expect(columns["email"].display_size or columns["email"].internal_size, 191, "users.email length")
        expect("auto_increment" in columns["id"].extra, True, "users.id auto_increment")

        expect(intro.get_primary_key_column(cursor, "users"), "id", "users primary key")
        expect(intro.get_relations(cursor, "posts"), {"user_id": ("id", "users")}, "posts relations")

        constraints = intro.get_constraints(cursor, "posts")
        print("posts constraints:", constraints)
        fks = [c for c in constraints.values() if c["foreign_key"]]
        expect([(c["columns"], c["foreign_key"]) for c in fks], [(["user_id"], ("users", "id"))], "posts FKs")
        expect(any(c["primary_key"] and c["columns"] == ["id"] for c in constraints.values()), True,
               "posts primary key")

        user_constraints = intro.get_constraints(cursor, "users")
        print("users constraints:", user_constraints)
        expect(any(c["unique"] and c["columns"] == ["email"] for c in user_constraints.values()), True,
               "users.email unique")

        pt = intro.get_constraints(cursor, "post_tag")
        print("post_tag constraints:", pt)
        expect(any(c["unique"] and not c["primary_key"] and c["columns"] == ["post_id", "tag_id"]
                   for c in pt.values()), True, "post_tag unique (post_id, tag_id)")
        expect(sorted(c["foreign_key"] for c in pt.values() if c["foreign_key"]),
               [("posts", "id"), ("tags", "id")], "post_tag FKs")


def insert() -> None:
    alice = User.objects.create(email="alice@example.com", name="Alice", balance=Decimal("100.50"),
                                profile={"city": "Tokyo", "tags": ["a", "b"]})
    bob = User.objects.create(email="bob@example.com", name="Bob", balance=Decimal("20.00"),
                              profile={"city": "Osaka"})
    carol = User.objects.create(email="carol@example.com", name="Carol", balance=Decimal("5.25"),
                                is_active=False)
    if not (alice.pk and bob.pk and carol.pk) or len({alice.pk, bob.pk, carol.pk}) != 3:
        raise AssertionError(f"auto-increment ids not returned: {alice.pk}, {bob.pk}, {carol.pk}")
    Tag.objects.bulk_create([Tag(name="python"), Tag(name="sql"), Tag(name="news")])
    tags = {t.name: t for t in Tag.objects.all()}
    p1 = Post.objects.create(user=alice, title="Hello", body="first post", views=10)
    p2 = Post.objects.create(user=alice, title="Django on MySQL", body="second", views=5)
    p3 = Post.objects.create(user=bob, title="Bob's post", body="hi", views=1)
    Post.objects.create(user=bob, title="Draft", body="unpublished", published_at=None)
    p1.tags.add(tags["python"], tags["news"])
    p2.tags.add(tags["python"], tags["sql"])
    p3.tags.set([tags["news"]])
    expect(User.objects.count(), 3, "users")
    expect(Post.objects.count(), 4, "posts")
    expect(Post.tags.through.objects.count(), 5, "post_tag rows")
    fetched = User.objects.get(pk=alice.pk)
    expect(fetched.balance, Decimal("100.50"), "alice balance")
    expect(fetched.profile, {"city": "Tokyo", "tags": ["a", "b"]}, "alice profile")
    expect(fetched.is_active, True, "alice is_active")
    if fetched.created_at is None:
        raise AssertionError("created_at not stored")


def relations() -> None:
    posts = list(Post.objects.select_related("user").prefetch_related("tags").order_by("id"))
    seen = [(p.title, p.user.name, sorted(t.name for t in p.tags.all())) for p in posts]
    print(seen)
    expect(seen, [
        ("Hello", "Alice", ["news", "python"]),
        ("Django on MySQL", "Alice", ["python", "sql"]),
        ("Bob's post", "Bob", ["news"]),
        ("Draft", "Bob", []),
    ], "posts with users and tags")

    users = User.objects.prefetch_related("posts").order_by("id")
    expect([(u.name, len(u.posts.all())) for u in users], [("Alice", 2), ("Bob", 2), ("Carol", 0)],
           "users with posts")

    python_authors = list(User.objects.filter(posts__tags__name="python").distinct()
                          .order_by("name").values_list("name", flat=True))
    expect(python_authors, ["Alice"], "authors of python posts (join)")

    tag_posts = list(Tag.objects.get(name="news").posts.order_by("id").values_list("title", flat=True))
    expect(tag_posts, ["Hello", "Bob's post"], "posts tagged news")


def update() -> None:
    changed = User.objects.filter(email="bob@example.com").update(balance=F("balance") + 10)
    expect(changed, 1, "rows updated")
    bob = User.objects.get(email="bob@example.com")
    expect(bob.balance, Decimal("30.00"), "bob balance")
    before = bob.updated_at
    bob.name = "Robert"
    bob.save()
    bob.refresh_from_db()
    expect(bob.name, "Robert", "bob name")
    if bob.updated_at < before:
        raise AssertionError("updated_at went backwards")
    expect(Post.objects.filter(views__lt=5).update(views=F("views") + 1), 2, "posts with few views")


def delete() -> None:
    temp = User.objects.create(email="temp@example.com", name="Temp")
    post = Post.objects.create(user=temp, title="to be deleted", body="x")
    post.tags.add(Tag.objects.get(name="sql"))
    deleted, per_model = temp.delete()
    print("deleted:", deleted, per_model)
    expect(Post.objects.filter(title="to be deleted").exists(), False, "cascaded post")
    expect(User.objects.filter(email="temp@example.com").exists(), False, "deleted user")
    expect(Tag.objects.filter(name="sql").exists(), True, "tag kept")
    Tag.objects.create(name="obsolete")
    expect(Tag.objects.filter(name="obsolete").delete()[0], 1, "deleted tags")


def pagination() -> None:
    paginator = Paginator(Post.objects.order_by("id"), 3)
    expect(paginator.count, 4, "total posts")
    expect(paginator.num_pages, 2, "pages")
    page = paginator.page(2)
    expect([p.title for p in page.object_list], ["Draft"], "page 2")
    expect(page.has_previous(), True, "page 2 has previous")
    expect([p.title for p in Post.objects.order_by("id")[1:3]], ["Django on MySQL", "Bob's post"],
           "slice with offset")


def aggregate() -> None:
    totals = User.objects.aggregate(n=Count("id"), total=Sum("balance"), avg=Avg("balance"))
    print(totals)
    expect(totals["n"], 3, "user count")
    expect(totals["total"], Decimal("135.75"), "balance sum")
    expect(round(totals["avg"], 2), Decimal("45.25"), "balance avg")

    busy = list(User.objects.annotate(post_count=Count("posts"), views=Sum("posts__views"))
                .filter(post_count__gte=2).order_by("id").values_list("name", "post_count", "views"))
    expect(busy, [("Alice", 2, 15), ("Robert", 2, 3)], "users with at least two posts (HAVING)")

    by_active = list(User.objects.values("is_active").annotate(n=Count("id"), total=Sum("balance"))
                     .order_by("is_active"))
    expect(by_active, [
        {"is_active": False, "n": 1, "total": Decimal("5.25")},
        {"is_active": True, "n": 2, "total": Decimal("130.50")},
    ], "group by is_active")

    tag_counts = list(Tag.objects.annotate(n=Count("posts")).filter(n__gt=0).order_by("-n", "name")
                      .values_list("name", "n"))
    expect(tag_counts, [("news", 2), ("python", 2), ("sql", 1)], "posts per tag")


def transaction_commit() -> None:
    with transaction.atomic():
        dave = User.objects.create(email="dave@example.com", name="Dave", balance=Decimal("1.00"))
        User.objects.filter(pk=dave.pk).update(balance=F("balance") + 1)
    expect(User.objects.get(email="dave@example.com").balance, Decimal("2.00"), "committed balance")


class Rollback(Exception):
    pass


def transaction_rollback() -> None:
    try:
        with transaction.atomic():
            User.objects.create(email="ghost@example.com", name="Ghost")
            raise Rollback()
    except Rollback:
        pass
    expect(User.objects.filter(email="ghost@example.com").exists(), False, "rolled back user")

    with transaction.atomic():
        User.objects.create(email="outer@example.com", name="Outer")
        try:
            with transaction.atomic():
                User.objects.create(email="inner@example.com", name="Inner")
                raise Rollback()
        except Rollback:
            pass
    expect(User.objects.filter(email="outer@example.com").exists(), True, "outer user after savepoint rollback")
    expect(User.objects.filter(email="inner@example.com").exists(), False, "inner user rolled back to savepoint")


def json_lookup() -> None:
    expect(list(User.objects.filter(profile__city="Tokyo").values_list("name", flat=True)), ["Alice"],
           "profile__city=Tokyo")
    expect(sorted(User.objects.filter(profile__has_key="city").values_list("name", flat=True)),
           ["Alice", "Robert"], "profile has key city")
    expect(list(User.objects.filter(profile__tags__contains=["a"]).values_list("name", flat=True)), ["Alice"],
           "profile tags contain a")
    expect(list(User.objects.filter(profile__isnull=True).values_list("name", flat=True).order_by("name")),
           ["Carol", "Dave", "Outer"], "profile is NULL")


def upsert() -> None:
    User.objects.bulk_create(
        [
            User(email="alice@example.com", name="Alice Updated", balance=Decimal("200.00")),
            User(email="erin@example.com", name="Erin", balance=Decimal("3.00")),
        ],
        update_conflicts=True,
        update_fields=["name", "balance"],
    )
    alice = User.objects.get(email="alice@example.com")
    expect((alice.name, alice.balance), ("Alice Updated", Decimal("200.00")), "alice after upsert")
    expect(User.objects.filter(email="erin@example.com").exists(), True, "erin inserted by upsert")

    frank, created = User.objects.update_or_create(email="frank@example.com",
                                                   defaults={"name": "Frank", "balance": Decimal("1.00")})
    expect(created, True, "frank created")
    frank, created = User.objects.update_or_create(email="frank@example.com",
                                                   defaults={"name": "Franklin"})
    expect((created, frank.name), (False, "Franklin"), "frank updated")
    tag, created = Tag.objects.get_or_create(name="python")
    expect(created, False, "existing tag found")


def alter_migration() -> None:
    source = APP_DIR / "blog" / "later_migrations" / "0002_post_slug_content_views.py"
    # The second migration ships the way a later deploy does: the file appears
    # in blog/migrations only now, so the first `migrate` stops at 0001.
    shutil.copy(source, APP_DIR / "blog" / "migrations" / source.name)
    importlib.invalidate_caches()
    command("migrate", "blog", "0002", interactive=False)
    expect(latest_blog_migration(), "0002_post_slug_content_views", "applied blog migration")
    columns, constraints = posts_schema()
    for name in ("slug", "content"):
        if name not in columns:
            raise AssertionError(f"posts.{name} missing after 0002: {sorted(columns)}")
    if "body" in columns:
        raise AssertionError("posts.body still present after rename")
    expect(bool(columns["slug"].null_ok), True, "posts.slug nullable")
    if "posts_slug_idx" not in constraints or constraints["posts_slug_idx"]["columns"] != ["slug"]:
        raise AssertionError(f"posts_slug_idx missing: {constraints}")
    field_type = connection.introspection.get_field_type(columns["views"].type_code, columns["views"])
    expect(field_type, "BigIntegerField", "posts.views type")
    with connection.cursor() as cursor:
        cursor.execute("SELECT COUNT(*) FROM posts WHERE content IS NOT NULL")
        expect(cursor.fetchone()[0], 4, "posts keep their body after the rename")


def rollback_migration() -> None:
    command("migrate", "blog", "0001", interactive=False)
    expect(latest_blog_migration(), "0001_initial", "applied blog migration")
    columns, constraints = posts_schema()
    expect(sorted(columns), ["body", "id", "published_at", "title", "user_id", "views"], "posts columns")
    if "posts_slug_idx" in constraints:
        raise AssertionError("posts_slug_idx survived the rollback")
    field_type = connection.introspection.get_field_type(columns["views"].type_code, columns["views"])
    expect(field_type, "IntegerField", "posts.views type")
    expect(Post.objects.filter(body="first post").count(), 1, "body readable through the ORM again")


def flush() -> None:
    command("flush", interactive=False)
    expect(User.objects.count(), 0, "users after flush")
    expect(Post.objects.count(), 0, "posts after flush")
    expect(Tag.objects.count(), 0, "tags after flush")
    # flush reruns post_migrate, which recreates content types.
    from django.contrib.contenttypes.models import ContentType

    if not ContentType.objects.filter(app_label="blog", model="user").exists():
        raise AssertionError("content types not recreated after flush")


def latest_blog_migration() -> str:
    with connection.cursor() as cursor:
        cursor.execute("SELECT name FROM django_migrations WHERE app = 'blog' ORDER BY id DESC LIMIT 1")
        row = cursor.fetchone()
    return row[0] if row else None


def posts_schema():
    intro = connection.introspection
    with connection.cursor() as cursor:
        columns = {c.name: c for c in intro.get_table_description(cursor, "posts")}
        constraints = intro.get_constraints(cursor, "posts")
    print("posts columns:", columns)
    print("posts constraints:", constraints)
    return columns, constraints


STEPS = [
    ("connect-default-charset", connect_default_charset),
    ("connect", connect),
    ("migrate", migrate),
    ("migrate-again", migrate_again),
    ("makemigrations-check", makemigrations_check),
    ("check", check),
    ("showmigrations", showmigrations),
    ("sqlmigrate", sqlmigrate),
    ("introspect", introspect),
    ("insert", insert),
    ("relations", relations),
    ("update", update),
    ("delete", delete),
    ("pagination", pagination),
    ("aggregate", aggregate),
    ("transaction-commit", transaction_commit),
    ("transaction-rollback", transaction_rollback),
    ("json", json_lookup),
    ("upsert", upsert),
    ("alter-migration", alter_migration),
    ("rollback-migration", rollback_migration),
    ("flush", flush),
]

if __name__ == "__main__":
    main()
