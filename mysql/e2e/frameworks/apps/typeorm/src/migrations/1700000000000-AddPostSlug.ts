import { MigrationInterface, QueryRunner, TableColumn, TableIndex } from 'typeorm';

export class AddPostSlug1700000000000 implements MigrationInterface {
  name = 'AddPostSlug1700000000000';

  async up(queryRunner: QueryRunner): Promise<void> {
    await queryRunner.addColumn(
      'posts',
      new TableColumn({ name: 'slug', type: 'varchar', length: '200', isNullable: true }),
    );
    await queryRunner.createIndex('posts', new TableIndex({ name: 'IDX_posts_slug', columnNames: ['slug'] }));
    await queryRunner.renameColumn('posts', 'views', 'view_count');
  }

  async down(queryRunner: QueryRunner): Promise<void> {
    await queryRunner.renameColumn('posts', 'view_count', 'views');
    await queryRunner.dropIndex('posts', 'IDX_posts_slug');
    await queryRunner.dropColumn('posts', 'slug');
  }
}
