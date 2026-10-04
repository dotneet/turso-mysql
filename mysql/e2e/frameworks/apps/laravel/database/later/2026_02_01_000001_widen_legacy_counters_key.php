<?php

use Illuminate\Database\Migrations\Migration;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\DB;
use Illuminate\Support\Facades\Schema;

// An app made with `increments` widens its INT UNSIGNED keys to the
// `bigIncrements` every new Laravel table has, which Laravel writes as a
// MODIFY of the key keeping its auto_increment.
return new class extends Migration
{
    public function up(): void
    {
        Schema::create('legacy_counters', function (Blueprint $table) {
            $table->increments('id');
            $table->string('name', 50);
        });
        DB::table('legacy_counters')->insert(['name' => 'first']);
        Schema::table('legacy_counters', function (Blueprint $table) {
            $table->bigIncrements('id')->change();
        });
    }

    public function down(): void
    {
        Schema::dropIfExists('legacy_counters');
    }
};
