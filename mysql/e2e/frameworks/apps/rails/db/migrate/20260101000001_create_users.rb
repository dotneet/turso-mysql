class CreateUsers < ActiveRecord::Migration[8.0]
  def change
    create_table :users do |t|
      t.string :email, null: false
      t.string :name, null: false, limit: 100
      t.decimal :balance, precision: 10, scale: 2, null: false, default: 0
      t.boolean :is_active, null: false, default: true
      t.json :profile
      t.timestamps
    end
    add_index :users, :email, unique: true
  end
end
