"""add posts.slug, rename posts.body to content

Revision ID: 0002
Revises: 0001
Create Date: 2026-09-28 00:00:01

"""
from typing import Sequence, Union

from alembic import op
import sqlalchemy as sa

revision: str = "0002"
down_revision: Union[str, Sequence[str], None] = "0001"
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    op.add_column("posts", sa.Column("slug", sa.String(length=200), nullable=True))
    op.create_index(op.f("ix_posts_slug"), "posts", ["slug"], unique=False)
    op.alter_column("posts", "body", new_column_name="content", existing_type=sa.Text(), existing_nullable=False)
    op.alter_column(
        "posts",
        "views",
        type_=sa.BigInteger(),
        existing_type=sa.Integer(),
        existing_nullable=False,
        existing_server_default="0",
    )


def downgrade() -> None:
    op.alter_column(
        "posts",
        "views",
        type_=sa.Integer(),
        existing_type=sa.BigInteger(),
        existing_nullable=False,
        existing_server_default="0",
    )
    op.alter_column("posts", "content", new_column_name="body", existing_type=sa.Text(), existing_nullable=False)
    op.drop_index(op.f("ix_posts_slug"), table_name="posts")
    op.drop_column("posts", "slug")
