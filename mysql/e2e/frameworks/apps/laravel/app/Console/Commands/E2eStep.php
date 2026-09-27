<?php

namespace App\Console\Commands;

use App\Jobs\CreateTag;
use App\Models\Post;
use App\Models\Tag;
use App\Models\User;
use Illuminate\Console\Command;
use Illuminate\Database\Events\QueryExecuted;
use Illuminate\Database\UniqueConstraintViolationException;
use Illuminate\Support\Arr;
use Illuminate\Support\Carbon;
use Illuminate\Support\Facades\Cache;
use Illuminate\Support\Facades\DB;
use Illuminate\Support\Facades\Schema;
use RuntimeException;
use Throwable;

// One step of the end-to-end run. run.sh calls `php artisan e2e:step <name>`
// once per step and records the exit status; the data each step expects is
// what the steps before it left behind.
class E2eStep extends Command
{
    protected $signature = 'e2e:step {name}';

    protected $description = 'Run one step of the framework end-to-end check';

    public function handle(): int
    {
        DB::listen(function (QueryExecuted $query) {
            $this->line('SQL> '.$query->sql.' '.json_encode($query->bindings).' ('.$query->time.' ms)');
        });
        $name = $this->argument('name');
        $method = lcfirst(str_replace('-', '', ucwords($name, '-')));
        if (! method_exists($this, $method)) {
            $this->error("unknown step {$name}");

            return self::FAILURE;
        }
        try {
            $this->{$method}();
        } catch (Throwable $e) {
            $this->line(get_class($e).': '.$e->getMessage());
            $this->line($e->getTraceAsString());
            // run.sh keeps only the tail of the output, so the message goes last.
            $this->line('FAILED: '.get_class($e).': '.$e->getMessage());

            return self::FAILURE;
        }
        $this->line("step {$name} ok");

        return self::SUCCESS;
    }

    private function connect(): void
    {
        DB::connection()->getPdo();
        $row = DB::selectOne('select version() as version, database() as db');
        $this->line("server {$row->version}, database {$row->db}");
        $this->check($row->db === 'laravel', "database() is {$row->db}");
    }

    private function sessionCharset(): void
    {
        $row = DB::selectOne(
            'select @@character_set_client as client, @@character_set_connection as conn, '
            .'@@character_set_results as results, @@collation_connection as collation, @@sql_mode as sql_mode'
        );
        $this->line(json_encode($row));
        $this->check($row->client === 'utf8mb4', "character_set_client is {$row->client}");
        $this->check($row->collation === 'utf8mb4_unicode_ci', "collation_connection is {$row->collation}");
        $this->check(str_contains($row->sql_mode, 'STRICT_TRANS_TABLES'), "sql_mode is {$row->sql_mode}");
    }

    private function introspect(): void
    {
        foreach (['users', 'posts', 'tags', 'post_tag', 'migrations', 'cache', 'jobs', 'sessions'] as $table) {
            $this->check(Schema::hasTable($table), "hasTable({$table}) is false");
        }
        $this->check(! Schema::hasTable('no_such_table'), 'hasTable(no_such_table) is true');

        $tables = collect(Schema::getTables())->where('schema', 'laravel')->pluck('name')->sort()->values()->all();
        $this->line('tables: '.json_encode($tables));
        $this->check(in_array('post_tag', $tables, true), 'getTables() misses post_tag');
        $this->check(Schema::getColumnListing('tags') === ['id', 'name'], 'getColumnListing(tags) is '.json_encode(Schema::getColumnListing('tags')));

        $users = collect(Schema::getColumns('users'))->keyBy('name');
        $this->line('users columns: '.json_encode($users->values()));
        $this->expect($users['id']['type'], 'bigint unsigned', 'users.id type');
        $this->expect($users['id']['auto_increment'], true, 'users.id auto_increment');
        $this->expect($users['email']['type'], 'varchar(255)', 'users.email type');
        $this->expect($users['balance']['type'], 'decimal(10,2)', 'users.balance type');
        $this->expect($users['balance']['default'], '0.00', 'users.balance default');
        $this->expect($users['is_active']['type'], 'tinyint(1)', 'users.is_active type');
        $this->expect($users['profile']['type_name'], 'json', 'users.profile type_name');
        $this->expect($users['profile']['nullable'], true, 'users.profile nullable');
        $this->expect($users['created_at']['type'], 'timestamp', 'users.created_at type');
        $this->expect($users['name']['collation'], 'utf8mb4_unicode_ci', 'users.name collation');
        $this->expect(Schema::getColumnType('users', 'profile'), 'json', 'getColumnType(users, profile)');

        $posts = collect(Schema::getColumns('posts'))->keyBy('name');
        $this->expect($posts['views']['default'], '0', 'posts.views default');
        $this->expect($posts['body']['type'], 'text', 'posts.body type');
        $this->expect($posts['published_at']['type'], 'datetime', 'posts.published_at type');
        $this->expect($posts['created_at']['type'], 'datetime', 'posts.created_at type');
        $this->check(Schema::hasColumns('posts', ['user_id', 'title', 'body', 'published_at', 'views']), 'posts misses a column');

        $indexes = collect(Schema::getIndexes('users'));
        $this->line('users indexes: '.json_encode($indexes));
        $this->check($indexes->contains(fn ($i) => $i['primary'] && $i['columns'] === ['id']), 'users has no primary key on id');
        $this->check($indexes->contains(fn ($i) => $i['name'] === 'users_email_unique' && $i['unique'] && $i['columns'] === ['email']), 'users has no unique index on email');
        $this->check(Schema::hasIndex('tags', ['name'], 'unique'), 'hasIndex(tags, name, unique) is false');
        $pivot = collect(Schema::getIndexes('post_tag'));
        $this->check($pivot->contains(fn ($i) => $i['primary'] && $i['columns'] === ['post_id', 'tag_id']), 'post_tag primary key is '.json_encode($pivot));

        $foreignKeys = collect(Schema::getForeignKeys('posts'));
        $this->line('posts foreign keys: '.json_encode($foreignKeys));
        $this->check($foreignKeys->contains(fn ($fk) => $fk['columns'] === ['user_id']
            && $fk['foreign_table'] === 'users'
            && $fk['foreign_columns'] === ['id']
            && $fk['on_delete'] === 'cascade'), 'posts has no cascading foreign key to users');
        $this->check(count(Schema::getForeignKeys('post_tag')) === 2, 'post_tag does not have two foreign keys');
    }

    private function insert(): void
    {
        $alice = User::create([
            'name' => 'Alice', 'email' => 'alice@example.com', 'password' => 'secret',
            'balance' => '100.50', 'is_active' => true,
            'profile' => ['city' => 'Tokyo', 'tags' => ['a', 'b'], 'age' => 30],
        ]);
        $bob = User::create([
            'name' => 'Bob', 'email' => 'bob@example.com', 'password' => 'secret',
            'balance' => '20.00', 'is_active' => false,
            'profile' => ['city' => 'Osaka', 'tags' => ['b'], 'age' => 25],
        ]);
        $carol = User::create(['name' => 'Carol', 'email' => 'carol@example.com', 'password' => 'secret']);
        $this->check(is_int($alice->id) && $alice->id > 0, 'alice id is '.var_export($alice->id, true));
        $this->check($bob->id > $alice->id && $carol->id > $bob->id, 'ids are not increasing');

        [$php, $laravel, $sql] = collect(['php', 'laravel', 'sql'])->map(fn ($n) => Tag::create(['name' => $n]))->all();

        $hello = $alice->posts()->create(['title' => 'Hello', 'body' => 'First post', 'published_at' => now(), 'views' => 10]);
        $hello->tags()->attach([$php->id, $laravel->id]);
        $second = $alice->posts()->create(['title' => 'Second', 'body' => 'Draft', 'views' => 5]);
        $second->tags()->sync([$laravel->id, $sql->id]);
        $second->tags()->sync([$sql->id]);
        $alice->posts()->create(['title' => 'Third', 'body' => 'No tags']);
        $bobOne = $bob->posts()->create(['title' => 'Bob one', 'body' => 'x', 'published_at' => now(), 'views' => 7]);
        $bobOne->tags()->attach($php);
        $bob->posts()->create(['title' => 'Bob two', 'body' => 'y', 'views' => 1]);
        $carol->posts()->create(['title' => 'Carol one', 'body' => 'z', 'views' => 3]);

        $this->expect(User::count(), 3, 'user count');
        $this->expect(Post::count(), 6, 'post count');
        $this->expect(DB::table('post_tag')->count(), 4, 'post_tag count');

        $fresh = User::where('email', 'alice@example.com')->firstOrFail();
        $this->expect($fresh->balance, '100.50', 'alice balance');
        $this->expect($fresh->is_active, true, 'alice is_active');
        $this->expect(Arr::sortRecursive($fresh->profile), ['age' => 30, 'city' => 'Tokyo', 'tags' => ['a', 'b']], 'alice profile (key order is not significant)');
        $this->check($fresh->created_at instanceof Carbon, 'created_at is not a date');
        $this->check(abs($fresh->created_at->diffInSeconds(now())) < 120, 'created_at is '.$fresh->created_at);
        $this->expect(User::where('email', 'carol@example.com')->value('is_active'), true, 'carol is_active default');
        $this->expect(User::where('email', 'carol@example.com')->value('balance'), '0.00', 'carol balance default');
        $this->expect(Post::where('title', 'Third')->value('views'), 0, 'views default');
        $this->check(Post::where('title', 'Third')->value('published_at') === null, 'published_at is not null');
    }

    private function uniqueViolation(): void
    {
        try {
            User::create(['name' => 'Alice again', 'email' => 'alice@example.com', 'password' => 'x']);
            throw new RuntimeException('a duplicate email was accepted');
        } catch (UniqueConstraintViolationException $e) {
            $this->line('refused as expected: '.$e->getMessage());
        }
        $tag = Tag::createOrFirst(['name' => 'php']);
        $this->expect($tag->name, 'php', 'createOrFirst name');
        $this->expect(Tag::count(), 3, 'tag count after createOrFirst');
    }

    private function relations(): void
    {
        $users = User::with(['posts' => fn ($q) => $q->orderBy('id'), 'posts.tags'])->orderBy('id')->get();
        $alice = $users->firstWhere('email', 'alice@example.com');
        $this->expect($alice->posts->pluck('title')->all(), ['Hello', 'Second', 'Third'], 'alice posts');
        $this->expect($alice->posts[0]->tags->pluck('name')->sort()->values()->all(), ['laravel', 'php'], 'Hello tags');
        $this->expect($alice->posts[1]->tags->pluck('name')->all(), ['sql'], 'Second tags after sync');

        $post = Post::with('user')->where('title', 'Bob one')->firstOrFail();
        $this->expect($post->user->email, 'bob@example.com', 'Bob one author');

        $counts = User::withCount('posts')->orderBy('id')->pluck('posts_count', 'name')->all();
        $this->expect($counts, ['Alice' => 3, 'Bob' => 2, 'Carol' => 1], 'withCount(posts)');

        $withSql = User::whereHas('posts.tags', fn ($q) => $q->where('name', 'sql'))->pluck('name')->all();
        $this->expect($withSql, ['Alice'], 'whereHas(posts.tags sql)');

        $phpPosts = Tag::where('name', 'php')->firstOrFail()->posts()->orderBy('title')->pluck('title')->all();
        $this->expect($phpPosts, ['Bob one', 'Hello'], 'php tag posts');

        $rows = DB::table('posts')
            ->join('users', 'users.id', '=', 'posts.user_id')
            ->leftJoin('post_tag', 'post_tag.post_id', '=', 'posts.id')
            ->leftJoin('tags', 'tags.id', '=', 'post_tag.tag_id')
            ->where('users.is_active', true)
            ->select('users.name', 'posts.title', 'tags.name as tag')
            ->orderBy('posts.id')
            ->orderBy('tags.name')
            ->get();
        $this->line('join: '.json_encode($rows));
        $this->expect(
            $rows->map(fn ($r) => "{$r->name}/{$r->title}/".($r->tag ?? '-'))->all(),
            ['Alice/Hello/laravel', 'Alice/Hello/php', 'Alice/Second/sql', 'Alice/Third/-', 'Carol/Carol one/-'],
            'join rows'
        );
    }

    private function update(): void
    {
        $alice = User::where('email', 'alice@example.com')->firstOrFail();
        $before = $alice->updated_at;
        $this->waitOneSecond();
        $alice->update(['name' => 'Alice A', 'balance' => '150.25']);
        $alice = $alice->fresh();
        $this->expect($alice->name, 'Alice A', 'renamed alice');
        $this->expect($alice->balance, '150.25', 'alice balance');
        $this->check($alice->updated_at->gt($before), "updated_at did not move: {$before} -> {$alice->updated_at}");

        $bobId = User::where('email', 'bob@example.com')->value('id');
        $this->expect(Post::where('user_id', $bobId)->increment('views', 5), 2, 'increment affected rows');
        $this->expect(Post::where('title', 'Bob one')->value('views'), 12, 'Bob one views');

        $published = Post::whereNull('published_at')->update(['published_at' => now()]);
        $this->expect($published, 4, 'published rows');
        $this->expect(Post::whereNull('published_at')->count(), 0, 'unpublished count');

        $this->expect(User::where('email', 'nobody@example.com')->update(['name' => 'x']), 0, 'update of no rows');
    }

    private function delete(): void
    {
        $this->check(Post::where('title', 'Third')->firstOrFail()->delete(), 'delete returned false');
        $this->check(! Post::where('title', 'Third')->exists(), 'Third still exists');

        $bob = User::where('email', 'bob@example.com')->firstOrFail();
        $bob->delete();
        $this->expect(Post::where('user_id', $bob->id)->count(), 0, 'posts of deleted user (ON DELETE CASCADE)');
        $this->expect(DB::table('post_tag')->count(), 3, 'post_tag rows after cascade');
        $this->expect(User::count(), 2, 'user count');
        $this->expect(Post::count(), 3, 'post count');
    }

    private function pagination(): void
    {
        $carol = User::where('email', 'carol@example.com')->firstOrFail();
        $carol->posts()->createMany(collect(range(1, 7))->map(fn ($i) => [
            'title' => "Page {$i}", 'body' => "page body {$i}", 'views' => $i,
        ])->all());
        $this->expect(Post::count(), 10, 'post count');

        $page = Post::orderBy('id')->paginate(4, ['*'], 'page', 2);
        $this->expect($page->total(), 10, 'paginate total');
        $this->expect($page->lastPage(), 3, 'paginate lastPage');
        $this->expect($page->count(), 4, 'paginate page size');
        $this->expect($page->first()->id, Post::orderBy('id')->skip(4)->value('id'), 'first id of page 2');

        $filtered = Post::where('title', 'like', 'Page %')->orderByDesc('views')->paginate(3);
        $this->expect($filtered->total(), 7, 'filtered total');
        $this->expect($filtered->pluck('views')->all(), [7, 6, 5], 'filtered first page');

        $simple = Post::orderBy('id')->simplePaginate(4, ['*'], 'page', 3);
        $this->expect($simple->count(), 2, 'simplePaginate last page size');
        $this->expect($simple->hasMorePages(), false, 'simplePaginate hasMorePages');

        $cursor = Post::orderBy('id')->cursorPaginate(4);
        $next = Post::orderBy('id')->cursorPaginate(4, ['*'], 'cursor', $cursor->nextCursor());
        $this->expect($next->first()->id, $page->first()->id, 'cursorPaginate second page');

        $this->expect(Post::orderBy('id')->skip(8)->take(5)->get()->count(), 2, 'skip/take');
    }

    private function aggregate(): void
    {
        $this->expect(User::count(), 2, 'count');
        $this->expect(round((float) User::sum('balance'), 2), 150.25, 'sum(balance)');
        $this->expect(round((float) User::avg('balance'), 3), 75.125, 'avg(balance)');
        $this->expect(Post::max('views'), 10, 'max(views)');
        $this->expect(Post::min('views'), 1, 'min(views)');
        $this->expect(User::where('is_active', true)->count(), 2, 'active count');

        $groups = Post::query()
            ->join('users', 'users.id', '=', 'posts.user_id')
            ->select('users.email', DB::raw('count(*) as post_count'), DB::raw('sum(posts.views) as total_views'))
            ->groupBy('users.email')
            ->having('post_count', '>=', 2)
            ->orderBy('users.email')
            ->get();
        $this->expect(
            $groups->map(fn ($g) => "{$g->email}:{$g->post_count}:{$g->total_views}")->all(),
            ['alice@example.com:2:15', 'carol@example.com:8:31'],
            'group by / having'
        );
        $big = Post::select('user_id')->groupBy('user_id')->havingRaw('count(*) > ?', [2])->count();
        $this->expect($big, 1, 'havingRaw count');

        $sums = User::withSum('posts', 'views')->withMax('posts', 'views')->orderBy('id')->get();
        $this->expect($sums->map(fn ($u) => (int) $u->posts_sum_views)->all(), [15, 31], 'withSum(posts.views)');
        $this->expect($sums->map(fn ($u) => (int) $u->posts_max_views)->all(), [10, 7], 'withMax(posts.views)');
    }

    private function transactionCommit(): void
    {
        DB::transaction(function () {
            $alice = User::where('email', 'alice@example.com')->lockForUpdate()->firstOrFail();
            $dave = User::create(['name' => 'Dave', 'email' => 'dave@example.com', 'password' => 'x', 'balance' => '0']);
            $alice->decrement('balance', 10);
            $dave->increment('balance', 10);
            $dave->posts()->create(['title' => 'In a transaction', 'body' => 'committed']);
        });
        $this->expect(User::where('email', 'dave@example.com')->value('balance'), '10.00', 'dave balance');
        $this->expect(User::where('email', 'alice@example.com')->value('balance'), '140.25', 'alice balance');
        $this->check(Post::where('title', 'In a transaction')->exists(), 'committed post is missing');
    }

    private function transactionRollback(): void
    {
        try {
            DB::transaction(function () {
                User::create(['name' => 'Eve', 'email' => 'eve@example.com', 'password' => 'x']);
                User::where('email', 'alice@example.com')->update(['balance' => '0']);
                throw new RuntimeException('roll back on purpose');
            });
        } catch (RuntimeException $e) {
            $this->expect($e->getMessage(), 'roll back on purpose', 'exception from the transaction');
        }
        $this->check(! User::where('email', 'eve@example.com')->exists(), 'eve survived the rollback');
        $this->expect(User::where('email', 'alice@example.com')->value('balance'), '140.25', 'alice balance after rollback');

        DB::beginTransaction();
        User::create(['name' => 'Frank', 'email' => 'frank@example.com', 'password' => 'x']);
        DB::rollBack();
        $this->check(! User::where('email', 'frank@example.com')->exists(), 'frank survived DB::rollBack()');
    }

    private function transactionSavepoint(): void
    {
        DB::transaction(function () {
            User::create(['name' => 'Grace', 'email' => 'grace@example.com', 'password' => 'x']);
            try {
                DB::transaction(function () {
                    User::create(['name' => 'Heidi', 'email' => 'heidi@example.com', 'password' => 'x']);
                    throw new RuntimeException('roll back the inner transaction');
                });
            } catch (RuntimeException) {
            }
            $this->expect(DB::transactionLevel(), 1, 'transaction level after the inner rollback');
        });
        $this->check(User::where('email', 'grace@example.com')->exists(), 'grace is missing');
        $this->check(! User::where('email', 'heidi@example.com')->exists(), 'heidi survived the savepoint rollback');
    }

    private function json(): void
    {
        $this->expect(User::where('profile->city', 'Tokyo')->pluck('email')->all(), ['alice@example.com'], "where('profile->city')");
        $this->expect(User::whereJsonContains('profile->tags', 'b')->pluck('email')->all(), ['alice@example.com'], 'whereJsonContains(profile->tags, b)');
        $this->expect(User::whereJsonDoesntContain('profile->tags', 'z')->pluck('email')->all(), ['alice@example.com'], 'whereJsonDoesntContain');
        $this->expect(User::whereJsonLength('profile->tags', 2)->count(), 1, 'whereJsonLength');
        $this->expect(User::where('profile->age', '>', 20)->count(), 1, "where('profile->age', '>', 20)");
        $this->expect(User::whereNull('profile')->count(), 3, 'users without a profile');

        User::where('email', 'alice@example.com')->update(['profile->city' => 'Kyoto']);
        $profile = User::where('email', 'alice@example.com')->firstOrFail()->profile;
        $this->expect(Arr::sortRecursive($profile), ['age' => 30, 'city' => 'Kyoto', 'tags' => ['a', 'b']], 'profile after a JSON path update');

        $cities = User::whereNotNull('profile')->select('email', 'profile->city as city')->get()->pluck('city', 'email')->all();
        $this->expect($cities, ['alice@example.com' => 'Kyoto'], 'select profile->city');
    }

    private function upsert(): void
    {
        $phpId = Tag::where('name', 'php')->value('id');
        Tag::upsert([['name' => 'php'], ['name' => 'rust']], ['name'], ['name']);
        $this->expect(Tag::where('name', 'php')->value('id'), $phpId, 'php id after upsert');
        $this->check(Tag::where('name', 'rust')->exists(), 'rust was not inserted');

        User::upsert([
            ['email' => 'alice@example.com', 'name' => 'Alice U', 'password' => 'x', 'balance' => '1.00'],
            ['email' => 'ivan@example.com', 'name' => 'Ivan', 'password' => 'x', 'balance' => '2.00'],
        ], ['email'], ['name', 'balance']);
        $alice = User::where('email', 'alice@example.com')->firstOrFail();
        $this->expect([$alice->name, $alice->balance], ['Alice U', '1.00'], 'alice after upsert');
        $this->expect(User::where('email', 'ivan@example.com')->value('balance'), '2.00', 'ivan after upsert');

        $go = Tag::updateOrCreate(['name' => 'go']);
        $this->check($go->wasRecentlyCreated, 'updateOrCreate did not create go');
        $php = Tag::firstOrCreate(['name' => 'php']);
        $this->check(! $php->wasRecentlyCreated && $php->id === $phpId, 'firstOrCreate created php again');

        $inserted = DB::table('tags')->insertOrIgnore([['name' => 'php'], ['name' => 'zig']]);
        $this->expect($inserted, 1, 'insertOrIgnore affected rows');
        $this->expect(Tag::orderBy('name')->pluck('name')->all(), ['go', 'laravel', 'php', 'rust', 'sql', 'zig'], 'tags');
    }

    private function cache(): void
    {
        $store = Cache::store('database');
        $store->put('greeting', ['hello' => 'world'], 600);
        $this->expect($store->get('greeting'), ['hello' => 'world'], 'cache get');
        $store->put('greeting', 'replaced', 600);
        $this->expect($store->get('greeting'), 'replaced', 'cache get after overwrite');
        $this->expect($store->remember('computed', 600, fn () => 42), 42, 'remember');
        $this->expect($store->remember('computed', 600, fn () => 0), 42, 'remember hit');
        $store->put('counter', 1, 600);
        $store->increment('counter', 2);
        $this->expect((int) $store->get('counter'), 3, 'increment');
        $this->check($store->add('once', 'first', 600), 'add of a new key failed');
        $this->check(! $store->add('once', 'second', 600), 'add of an existing key succeeded');
        $store->forget('greeting');
        $this->check($store->get('greeting') === null, 'forgotten key is still there');

        $lock = $store->lock('report', 10);
        $this->check($lock->get(), 'could not take the lock');
        $this->check(! $store->lock('report', 10)->get(), 'took a held lock twice');
        $lock->release();
        $this->check($store->lock('report', 10)->get(), 'could not take the released lock');
    }

    private function queueDispatch(): void
    {
        CreateTag::dispatch('from-queue');
        $this->expect(DB::table('jobs')->count(), 1, 'queued jobs');
    }

    private function queueCheck(): void
    {
        $this->check(Tag::where('name', 'from-queue')->exists(), 'the queued job did not run');
        $this->expect(DB::table('jobs')->count(), 0, 'jobs left');
        $this->expect(DB::table('failed_jobs')->count(), 0, 'failed jobs');
    }

    private function checkAlter(): void
    {
        $this->check(Schema::hasColumn('posts', 'slug'), 'posts.slug is missing');
        $this->check(Schema::hasColumn('posts', 'view_count'), 'posts.view_count is missing');
        $this->check(! Schema::hasColumn('posts', 'views'), 'posts.views is still there');
        $columns = collect(Schema::getColumns('posts'))->keyBy('name');
        $this->expect($columns['title']['type'], 'varchar(500)', 'posts.title type');
        $this->expect($columns['slug']['nullable'], true, 'posts.slug nullable');
        $this->expect($columns['view_count']['default'], '0', 'posts.view_count default');
        $this->expect(Schema::getColumnListing('posts')[3] ?? null, 'slug', 'slug position (after title)');
        $this->check(Schema::hasIndex('posts', 'posts_slug_index'), 'posts_slug_index is missing');

        $this->expect(DB::table('posts')->where('title', 'Hello')->value('view_count'), 10, 'Hello view_count kept');
        DB::table('posts')->where('title', 'Hello')->update(['slug' => 'hello', 'title' => str_repeat('t', 400)]);
        $this->expect(DB::table('posts')->where('slug', 'hello')->value('view_count'), 10, 'lookup by slug');
        DB::table('posts')->where('slug', 'hello')->update(['title' => 'Hello']);
    }

    private function checkRollback(): void
    {
        $this->check(! Schema::hasColumn('posts', 'slug'), 'posts.slug is still there');
        $this->check(Schema::hasColumn('posts', 'views'), 'posts.views is missing');
        $this->check(! Schema::hasIndex('posts', 'posts_slug_index'), 'posts_slug_index is still there');
        $this->expect(collect(Schema::getColumns('posts'))->firstWhere('name', 'title')['type'], 'varchar(255)', 'posts.title type');
        $this->expect(Post::where('title', 'Hello')->value('views'), 10, 'Hello views kept');
        $this->check(! DB::table('migrations')->where('migration', 'like', '%add_slug%')->exists(), 'the rolled back migration is still recorded');
    }

    private function checkFresh(): void
    {
        foreach (['users', 'posts', 'tags', 'post_tag', 'cache', 'jobs'] as $table) {
            $this->expect(DB::table($table)->count(), 0, "{$table} rows after migrate:fresh");
        }
        $this->expect(DB::table('migrations')->count(), count(glob(database_path('migrations/*.php'))), 'migrations recorded');
        $this->check(Schema::hasColumn('posts', 'slug'), 'posts.slug is missing after migrate:fresh');
    }

    private function checkSeed(): void
    {
        $user = User::where('email', 'test@example.com')->firstOrFail();
        $this->expect($user->name, 'Test User', 'seeded user');
        $this->check($user->email_verified_at !== null, 'seeded user is not verified');
    }

    // updated_at has one-second precision; without the wait an update in the
    // same second as the insert cannot be told apart from no update at all.
    private function waitOneSecond(): void
    {
        usleep(1_100_000);
    }

    private function check(bool $condition, string $message): void
    {
        if (! $condition) {
            throw new RuntimeException($message);
        }
    }

    private function expect(mixed $actual, mixed $expected, string $what): void
    {
        if ($actual !== $expected) {
            throw new RuntimeException("{$what}: expected ".json_encode($expected).', got '.json_encode($actual));
        }
    }
}
