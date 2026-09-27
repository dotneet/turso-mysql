# Runs the named steps in one Rails process. Each step appends
# {"step","ok","error"} to $E2E_OUT/steps.jsonl and a failed step never stops
# the ones after it. With --check the steps are not recorded and the process
# exits 1 on the first failure, so run.sh can use them inside a larger step.
require_relative "../config/environment"
require "json"

class StepFailed < StandardError; end

def main(args)
  check_only = args.delete("--check")
  args.each do |name|
    method_name = "step_#{name.tr('-', '_')}"
    if check_only
      send(method_name)
    else
      record(name) { send(method_name) }
    end
  end
end

def record(name)
  puts "=== step #{name} at #{Time.now.utc.strftime('%Y-%m-%dT%H:%M:%S.%NZ')}"
  error = nil
  begin
    yield
  rescue Exception => e # rubocop:disable Lint/RescueException
    error = "#{e.class}: #{e.message}\n#{e.backtrace.first(20).join("\n")}"
    puts "step #{name} FAILED: #{error}"
  end
  line = { step: name, ok: error.nil? }
  line[:error] = error.length > 2000 ? error[-2000..] : error if error
  File.open(File.join(ENV.fetch("E2E_OUT"), "steps.jsonl"), "a") { |f| f.puts(JSON.generate(line)) }
end

def check(condition, message)
  raise StepFailed, message unless condition
end

def conn
  ActiveRecord::Base.connection
end

# mysql2 asks for its own default charset in the handshake when the config
# names no encoding.
def step_connect_mysql2_default_encoding
  puts "mysql2 #{Mysql2::VERSION}, client library #{Mysql2::Client.info.inspect}"
  client = Mysql2::Client.new(
    host: ENV.fetch("E2E_HOST"), port: Integer(ENV.fetch("E2E_PORT")), username: ENV.fetch("E2E_USER"),
    password: ENV.fetch("E2E_PASSWORD"), database: ENV.fetch("E2E_APP"),
    sslca: ENV.fetch("E2E_CA"), ssl_mode: :verify_identity
  )
  row = client.query("SELECT @@character_set_client AS client, @@collation_connection AS collation, " \
                     "@@character_set_results AS results").first
  puts "default encoding session: #{row.inspect}, client.encoding=#{client.encoding}"
  client.close
end

def step_connect
  row = conn.select_one("SELECT VERSION() AS version, @@character_set_client AS charset, " \
                        "@@collation_connection AS collation, @@sql_mode AS sql_mode")
  puts "connected: #{row.inspect}"
  check row["charset"] == "utf8mb4", "session charset is #{row['charset']}"
  check conn.active?, "connection is not active"
end

def step_introspect
  tables = conn.tables.sort
  puts "tables: #{tables.inspect}"
  check tables == %w[ar_internal_metadata posts posts_tags schema_migrations tags users], "tables are #{tables.inspect}"
  conn.columns(:users).each do |c|
    puts "users.#{c.name}: sql_type=#{c.sql_type} type=#{c.type} null=#{c.null} default=#{c.default.inspect} " \
         "limit=#{c.limit.inspect} precision=#{c.precision.inspect} scale=#{c.scale.inspect}"
  end
  users = conn.columns(:users).index_by(&:name)
  check users.keys == %w[id email name balance is_active profile created_at updated_at], "users columns #{users.keys}"
  check users["balance"].type == :decimal && users["balance"].precision == 10 && users["balance"].scale == 2,
        "balance is #{users['balance'].sql_type}"
  check users["is_active"].type == :boolean, "is_active is #{users['is_active'].sql_type}"
  check users["profile"].type == :json, "profile is #{users['profile'].sql_type}"
  check users["created_at"].type == :datetime && users["created_at"].precision == 6, "created_at is #{users['created_at'].sql_type}"
  check users["id"].sql_type == "bigint" && users["id"].auto_increment?, "id is #{users['id'].sql_type}"
  check conn.primary_key(:users) == "id", "primary key of users is #{conn.primary_key(:users).inspect}"
  check conn.primary_key(:posts_tags).nil?, "posts_tags has a primary key"
  %i[users posts tags posts_tags].each do |table|
    conn.indexes(table).each { |i| puts "#{table} index #{i.name} #{i.columns.inspect} unique=#{i.unique}" }
    conn.foreign_keys(table).each do |fk|
      puts "#{table} foreign key #{fk.name} #{fk.column} -> #{fk.to_table}.#{fk.primary_key} on_delete=#{fk.on_delete.inspect}"
    end
  end
  check conn.indexes(:users).any? { |i| i.columns == ["email"] && i.unique }, "no unique index on users.email"
  check conn.index_exists?(:posts, :user_id), "no index on posts.user_id"
  check conn.index_exists?(:posts_tags, [:post_id, :tag_id], unique: true), "no unique index on posts_tags"
  fks = conn.foreign_keys(:posts)
  check fks.size == 1 && fks[0].to_table == "users" && fks[0].on_delete == :cascade, "posts foreign keys #{fks.inspect}"
  check conn.foreign_keys(:posts_tags).map(&:to_table).sort == %w[posts tags], "posts_tags foreign keys"
  check User.columns_hash["balance"].type == :decimal, "model sees balance as #{User.columns_hash['balance'].type}"
end

def step_insert
  ruby = Tag.create!(name: "ruby")
  sql = Tag.create!(name: "sql")
  alice = User.create!(email: "alice@example.com", name: "Alice", balance: "100.50", is_active: true,
                       profile: { city: "Paris", tags: %w[a b] })
  alice.posts.create!(title: "Hello", body: "first post", views: 10, tags: [ruby, sql])
  again = alice.posts.create!(title: "Again", body: "second post", views: 5,
                              published_at: Time.utc(2026, 1, 2, 3, 4, 5))
  again.tags << Tag.create!(name: "news")
  bob = User.create!(email: "bob@example.com", name: "Bob", balance: 20, is_active: false, profile: { city: "Berlin" })
  bob.posts.create!(title: "Bob's post", body: "text", views: 1)
  User.create!(email: "carol@example.com", name: "Carol", is_active: true)

  got = User.find_by!(email: "alice@example.com")
  check got.name == "Alice" && got.balance == BigDecimal("100.5") && got.is_active, "read back #{got.attributes}"
  check got.created_at.present? && got.profile == { "city" => "Paris", "tags" => %w[a b] }, "read back #{got.attributes}"
  check again.reload.published_at == Time.utc(2026, 1, 2, 3, 4, 5), "published_at #{again.published_at.inspect}"
  check User.find_by!(email: "carol@example.com").balance.zero?, "default balance is not 0"
  duplicate = User.new(email: "alice@example.com", name: "Again")
  check !duplicate.save, "the uniqueness validation accepted a duplicate email"
  begin
    User.insert!({ email: "alice@example.com", name: "Again" })
    raise StepFailed, "the unique index accepted a duplicate email"
  rescue ActiveRecord::RecordNotUnique => e
    puts "duplicate refused as expected: #{e.message}"
  end
end

def step_relations
  users = User.includes(posts: :tags).order(:id).to_a
  check users.size == 3, "includes returned #{users.size} users"
  alice = users.first
  check alice.posts.size == 2, "alice has #{alice.posts.size} posts"
  check alice.posts.min_by(&:id).tags.map(&:name).sort == %w[ruby sql], "first post tags"
  check alice.tags.pluck(:name).sort == %w[news ruby sql], "alice's tags through posts: #{alice.tags.pluck(:name)}"
  active = Post.joins(:user).where(users: { is_active: true }).order(:id).pluck(:title)
  check active == %w[Hello Again], "joins returned #{active.inspect}"
  tagged = Post.eager_load(:user, :tags).where(tags: { name: "ruby" }).to_a
  check tagged.size == 1 && tagged[0].user.name == "Alice", "eager_load returned #{tagged.map(&:title)}"
  check Tag.joins(:posts).distinct.count == 3, "distinct tags on posts"
  check Post.where.missing(:tags).pluck(:title) == ["Bob's post"], "posts without tags"
end

def step_update
  bob = User.find_by!(email: "bob@example.com")
  before = bob.updated_at
  bob.update!(name: "Robert")
  check bob.reload.updated_at > before, "updated_at did not move"
  Post.where(user: bob).update_all("views = views + 3")
  User.where(id: bob.id).update_all(["balance = balance + ?", BigDecimal("5.75")])
  bob.reload.update!(is_active: true)
  bob.posts.first.increment!(:views)
  bob.reload
  check bob.name == "Robert" && bob.balance == BigDecimal("25.75") && bob.is_active, "bob is #{bob.attributes}"
  check bob.posts.first.views == 5, "views is #{bob.posts.first.views}"
end

def step_delete
  carol = User.find_by!(email: "carol@example.com")
  post = carol.posts.create!(title: "to cascade", tags: [Tag.create!(name: "doomed")])
  post.tags.clear
  carol.destroy!
  check Post.where(user_id: carol.id).count.zero?, "ON DELETE CASCADE left posts behind"
  check Tag.where(name: "doomed").delete_all == 1, "delete_all of the tag"
  check User.where(email: "nobody@example.com").delete_all.zero?, "delete_all of a missing row"
end

def step_pagination
  alice = User.find_by!(email: "alice@example.com")
  5.times { |i| alice.posts.create!(title: "page #{i}") }
  check Post.count == 8, "total is #{Post.count}"
  page = Post.order(:id).limit(2).offset(2)
  check page.to_a.size == 2 && page.count == 2, "page has #{page.to_a.size} rows, count #{page.count}"
  titles = Post.where("title LIKE ?", "page %").order(id: :desc).limit(3).pluck(:title)
  check titles == ["page 4", "page 3", "page 2"], "titles #{titles.inspect}"
  check Post.order(:id).offset(7).first(5).size == 1, "last page"
  check Post.exists?(title: "page 0"), "exists?"
end

def step_aggregate
  alice = User.find_by!(email: "alice@example.com")
  bob = User.find_by!(email: "bob@example.com")
  counts = Post.group(:user_id).having("COUNT(*) > ?", 1).count
  check counts == { alice.id => 7 }, "group/having count #{counts.inspect}"
  sums = Post.group(:user_id).order(:user_id).sum(:views)
  check sums == { alice.id => 15, bob.id => 5 }, "group sum #{sums.inspect}"
  average = Post.where(user: alice).average(:views)
  check (average - BigDecimal("2.1428")).abs < BigDecimal("0.001"), "average #{average.inspect}"
  total = User.where(is_active: true).sum(:balance)
  check total == BigDecimal("126.25"), "sum of balance #{total.inspect}"
  check User.maximum(:balance) == BigDecimal("100.5"), "maximum #{User.maximum(:balance).inspect}"
  check Post.distinct.count(:user_id) == 2, "distinct count"
  stats = Post.group(:user_id).pluck(:user_id, Arel.sql("COUNT(*)"), Arel.sql("MAX(views)"))
  check stats.sort == [[alice.id, 7, 10], [bob.id, 1, 5]], "pluck stats #{stats.inspect}"
end

def step_transaction_commit
  ActiveRecord::Base.transaction do
    alice = User.find_by!(email: "alice@example.com").lock!
    bob = User.find_by!(email: "bob@example.com").lock!
    alice.update!(balance: alice.balance - 10)
    bob.update!(balance: bob.balance + 10)
  end
  check User.find_by!(email: "bob@example.com").balance == BigDecimal("35.75"), "bob's balance after commit"
  check User.find_by!(email: "alice@example.com").balance == BigDecimal("90.5"), "alice's balance after commit"
end

def step_transaction_rollback
  ActiveRecord::Base.transaction do
    Tag.create!(name: "rolled-back")
    raise ActiveRecord::Rollback
  end
  check !Tag.exists?(name: "rolled-back"), "ActiveRecord::Rollback kept the row"
  begin
    ActiveRecord::Base.transaction do
      Tag.create!(name: "raised")
      raise ArgumentError, "roll back on purpose"
    end
  rescue ArgumentError
    nil
  end
  check !Tag.exists?(name: "raised"), "an exception kept the row"
end

def step_savepoint
  ActiveRecord::Base.transaction do
    Tag.create!(name: "outer")
    ActiveRecord::Base.transaction(requires_new: true) do
      Tag.create!(name: "nested")
      raise ActiveRecord::Rollback
    end
  end
  names = Tag.where(name: %w[outer nested]).pluck(:name)
  check names == ["outer"], "after the savepoint rollback the tags are #{names.inspect}"
end

def step_json
  check User.where("profile->>'$.city' = ?", "Paris").pluck(:name) == ["Alice"], "->> query"
  check User.where("JSON_CONTAINS(profile, ?, '$.tags')", '"a"').count == 1, "JSON_CONTAINS"
  cities = User.where("JSON_EXTRACT(profile, '$.city') IS NOT NULL").order(:id).pluck(Arel.sql("profile->>'$.city'"))
  check cities == %w[Paris Berlin], "cities #{cities.inspect}"
  alice = User.find_by!(email: "alice@example.com")
  alice.update!(profile: alice.profile.merge("city" => "Lyon"))
  check alice.reload.profile["city"] == "Lyon" && alice.profile["tags"] == %w[a b], "profile #{alice.profile.inspect}"
end

def step_upsert
  User.upsert_all([
    { email: "alice@example.com", name: "Alice Updated", balance: 1, is_active: true },
    { email: "dave@example.com", name: "Dave", balance: 2, is_active: true }
  ], update_only: %i[name balance])
  alice = User.find_by!(email: "alice@example.com")
  check alice.name == "Alice Updated" && alice.balance == 1, "alice after upsert #{alice.attributes}"
  check User.count == 3 && User.exists?(email: "dave@example.com"), "users after upsert #{User.pluck(:email)}"
  Tag.upsert({ name: "ruby" })
  check Tag.where(name: "ruby").count == 1, "Tag.upsert duplicated a tag"
end

def step_insert_all
  Tag.insert_all([{ name: "bulk-1" }, { name: "bulk-2" }, { name: "ruby" }])
  check Tag.where("name LIKE 'bulk-%'").count == 2, "insert_all rows"
  check Tag.where(name: "ruby").count == 1, "insert_all duplicated a tag"
  Tag.insert_all!([{ name: "bulk-3" }])
  begin
    Tag.insert_all!([{ name: "bulk-3" }])
    raise StepFailed, "insert_all! accepted a duplicate"
  rescue ActiveRecord::RecordNotUnique
    nil
  end
end

def step_after_alter
  Post.reset_column_information
  columns = Post.columns_hash
  check columns.key?("slug") && columns.key?("content") && !columns.key?("body"), "posts columns #{columns.keys}"
  check columns["views"].sql_type == "bigint", "views is #{columns['views'].sql_type}"
  check conn.index_exists?(:posts, :slug), "no index on posts.slug"
  Post.where(slug: nil).update_all("slug = CONCAT('post-', id)")
  check Post.where(slug: "post-1").exists?, "slug was not filled"
end

def step_after_rollback
  Post.reset_column_information
  columns = Post.columns_hash
  check !columns.key?("slug") && !columns.key?("content") && columns.key?("body"), "posts columns #{columns.keys}"
  check columns["views"].sql_type == "int", "views is #{columns['views'].sql_type}"
  check !conn.index_exists?(:posts, :slug), "the index on posts.slug is still there"
end

def step_drop_tables
  conn.disable_referential_integrity do
    conn.tables.each { |table| conn.drop_table(table, force: :cascade) }
  end
  check conn.tables.empty?, "tables left: #{conn.tables.inspect}"
end

def step_after_schema_load
  tables = conn.tables.sort
  check tables == %w[ar_internal_metadata posts posts_tags schema_migrations tags users], "tables are #{tables.inspect}"
  check conn.foreign_keys(:posts).size == 1, "posts foreign keys after load"
  user = User.create!(email: "loaded@example.com", name: "Loaded")
  user.posts.create!(title: "after load", tags: [Tag.create!(name: "loaded")])
  check Post.joins(:tags).where(tags: { name: "loaded" }).count == 1, "join after load"
end

main(ARGV.dup)
