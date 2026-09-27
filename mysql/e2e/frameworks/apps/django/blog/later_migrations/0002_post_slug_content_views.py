from django.db import migrations, models


class Migration(migrations.Migration):

    dependencies = [
        ("blog", "0001_initial"),
    ]

    operations = [
        migrations.AddField(
            model_name="post",
            name="slug",
            field=models.CharField(blank=True, max_length=200, null=True),
        ),
        migrations.AddIndex(
            model_name="post",
            index=models.Index(fields=["slug"], name="posts_slug_idx"),
        ),
        migrations.RenameField(
            model_name="post",
            old_name="body",
            new_name="content",
        ),
        migrations.AlterField(
            model_name="post",
            name="views",
            field=models.BigIntegerField(default=0),
        ),
    ]
