class WidenLegacyCountersKey < ActiveRecord::Migration[8.0]
  # An app made before Rails counted ids in BIGINT widens its INT keys with
  # change_column, which Rails writes as a CHANGE of the key keeping its
  # AUTO_INCREMENT.
  def up
    create_table :legacy_counters, id: :integer do |t|
      t.string :name, null: false, limit: 50
    end
    execute "INSERT INTO legacy_counters (name) VALUES ('first')"
    change_column :legacy_counters, :id, :bigint
  end

  def down
    drop_table :legacy_counters
  end
end
