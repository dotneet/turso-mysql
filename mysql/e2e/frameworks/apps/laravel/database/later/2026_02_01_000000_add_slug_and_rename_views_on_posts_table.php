<?php

use Illuminate\Database\Migrations\Migration;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\Schema;

// run.sh copies this into database/migrations/ in the alter-migration step,
// the way a developer adds a migration to an app that is already migrated.
return new class extends Migration
{
    public function up(): void
    {
        Schema::table('posts', function (Blueprint $table) {
            $table->string('slug')->nullable()->after('title');
            $table->index('slug');
        });
        Schema::table('posts', function (Blueprint $table) {
            $table->renameColumn('views', 'view_count');
            $table->string('title', 500)->change();
        });
    }

    public function down(): void
    {
        Schema::table('posts', function (Blueprint $table) {
            $table->string('title')->change();
            $table->renameColumn('view_count', 'views');
        });
        Schema::table('posts', function (Blueprint $table) {
            $table->dropIndex(['slug']);
            $table->dropColumn('slug');
        });
    }
};
