'use strict';

// Copied into migrations/ by the alter-migration step, the way a later
// release ships a new migration.
/** @type {import('sequelize-cli').Migration} */
module.exports = {
  async up(queryInterface, Sequelize) {
    await queryInterface.addColumn('posts', 'slug', { type: Sequelize.STRING(220), allowNull: true });
    await queryInterface.renameColumn('posts', 'views', 'view_count');
    await queryInterface.changeColumn('posts', 'title', { type: Sequelize.STRING(255), allowNull: false });
    await queryInterface.addIndex('posts', ['slug'], { name: 'posts_slug', unique: true });
  },

  async down(queryInterface, Sequelize) {
    await queryInterface.removeIndex('posts', 'posts_slug');
    await queryInterface.changeColumn('posts', 'title', { type: Sequelize.STRING(200), allowNull: false });
    await queryInterface.renameColumn('posts', 'view_count', 'views');
    await queryInterface.removeColumn('posts', 'slug');
  },
};
