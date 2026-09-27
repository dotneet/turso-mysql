class CreatePosts < ActiveRecord::Migration[8.0]
  def change
    create_table :posts do |t|
      t.references :user, null: false, foreign_key: { on_delete: :cascade }
      t.string :title, null: false, limit: 200
      t.text :body
      t.datetime :published_at
      t.integer :views, null: false, default: 0
      t.timestamps
    end
  end
end
