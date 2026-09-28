DROP TABLE `comments`;--> statement-breakpoint
ALTER TABLE `posts` DROP INDEX `posts_slug`;--> statement-breakpoint
ALTER TABLE `posts` MODIFY COLUMN `title` varchar(200) NOT NULL;--> statement-breakpoint
ALTER TABLE `posts` DROP COLUMN `slug`;