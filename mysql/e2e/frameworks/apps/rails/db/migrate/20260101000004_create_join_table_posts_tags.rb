class CreateJoinTablePostsTags < ActiveRecord::Migration[8.0]
  def change
    create_join_table :posts, :tags, column_options: { foreign_key: true } do |t|
      t.index [:post_id, :tag_id], unique: true
    end
  end
end
