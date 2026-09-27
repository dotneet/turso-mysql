require_relative "boot"
require "rails"
require "active_record/railtie"

Bundler.require(*Rails.groups)

# A Rails app with only Active Record, so `rake db:*` runs the same tasks a
# generated Rails app runs.
module E2e
  class Application < Rails::Application
    config.load_defaults 8.0
    config.eager_load = false
    config.logger = ActiveSupport::Logger.new($stdout)
    config.log_level = :debug
    config.colorize_logging = false
    config.active_record.verbose_query_logs = false
  end
end
