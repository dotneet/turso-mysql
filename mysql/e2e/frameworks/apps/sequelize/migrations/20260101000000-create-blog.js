'use strict';

/** @type {import('sequelize-cli').Migration} */
module.exports = {
  async up(queryInterface, Sequelize) {
    await queryInterface.createTable('users', {
      id: { type: Sequelize.BIGINT, autoIncrement: true, primaryKey: true },
      email: { type: Sequelize.STRING(191), allowNull: false, unique: true },
      name: { type: Sequelize.STRING(100), allowNull: false },
      balance: { type: Sequelize.DECIMAL(10, 2), allowNull: false, defaultValue: 0 },
      is_active: { type: Sequelize.BOOLEAN, allowNull: false, defaultValue: true },
      profile: { type: Sequelize.JSON, allowNull: true },
      version: { type: Sequelize.INTEGER, allowNull: false, defaultValue: 0 },
      created_at: { type: Sequelize.DATE, allowNull: false },
      updated_at: { type: Sequelize.DATE, allowNull: false },
    });
    await queryInterface.createTable('posts', {
      id: { type: Sequelize.BIGINT, autoIncrement: true, primaryKey: true },
      user_id: {
        type: Sequelize.BIGINT,
        allowNull: false,
        references: { model: 'users', key: 'id' },
        onDelete: 'CASCADE',
        onUpdate: 'CASCADE',
      },
      title: { type: Sequelize.STRING(200), allowNull: false },
      body: { type: Sequelize.TEXT, allowNull: true },
      published_at: { type: Sequelize.DATE, allowNull: true },
      views: { type: Sequelize.INTEGER, allowNull: false, defaultValue: 0 },
      created_at: { type: Sequelize.DATE, allowNull: false },
      updated_at: { type: Sequelize.DATE, allowNull: false },
    });
    await queryInterface.addIndex('posts', ['user_id', 'published_at'], { name: 'posts_user_published' });
    await queryInterface.createTable('tags', {
      id: { type: Sequelize.BIGINT, autoIncrement: true, primaryKey: true },
      name: { type: Sequelize.STRING(100), allowNull: false, unique: true },
    });
    await queryInterface.createTable('post_tags', {
      post_id: {
        type: Sequelize.BIGINT,
        allowNull: false,
        primaryKey: true,
        references: { model: 'posts', key: 'id' },
        onDelete: 'CASCADE',
      },
      tag_id: {
        type: Sequelize.BIGINT,
        allowNull: false,
        primaryKey: true,
        references: { model: 'tags', key: 'id' },
        onDelete: 'CASCADE',
      },
    });
  },

  async down(queryInterface) {
    await queryInterface.dropTable('post_tags');
    await queryInterface.dropTable('tags');
    await queryInterface.dropTable('posts');
    await queryInterface.dropTable('users');
  },
};
