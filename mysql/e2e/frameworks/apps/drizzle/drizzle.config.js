// drizzle-kit reads this. DRIZZLE_SCHEMA and DRIZZLE_OUT pick the schema
// release and its migrations folder, so one config serves both releases.
const fs = require('node:fs');

const env = process.env;

module.exports = {
  dialect: 'mysql',
  schema: env.DRIZZLE_SCHEMA || './src/schema.js',
  out: env.DRIZZLE_OUT || './drizzle-v1',
  dbCredentials: {
    host: env.E2E_HOST || '127.0.0.1',
    port: Number(env.E2E_PORT || 3306),
    user: env.E2E_USER,
    password: env.E2E_PASSWORD,
    database: env.E2E_APP || 'drizzle',
    ssl: env.E2E_CA ? { ca: fs.readFileSync(env.E2E_CA, 'utf8') } : undefined,
  },
  verbose: true,
  strict: false,
};
