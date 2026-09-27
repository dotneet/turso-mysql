<?php

namespace App\Jobs;

use App\Models\Tag;
use Illuminate\Contracts\Queue\ShouldQueue;
use Illuminate\Foundation\Queue\Queueable;

class CreateTag implements ShouldQueue
{
    use Queueable;

    public function __construct(public string $name)
    {
    }

    public function handle(): void
    {
        Tag::firstOrCreate(['name' => $this->name]);
    }
}
