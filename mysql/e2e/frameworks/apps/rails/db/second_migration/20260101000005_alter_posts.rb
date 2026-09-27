class AlterPosts < ActiveRecord::Migration[8.0]
  def up
    add_column :posts, :slug, :string, limit: 200, null: true
    add_index :posts, :slug
    rename_column :posts, :body, :content
    change_column :posts, :views, :bigint, null: false, default: 0
  end

  def down
    change_column :posts, :views, :integer, null: false, default: 0
    rename_column :posts, :content, :body
    remove_index :posts, :slug
    remove_column :posts, :slug
  end
end
