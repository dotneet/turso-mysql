// sequelize-cli reads this for db:migrate; app.js uses the same settings.
const fs = require('node:fs');

const env = process.env;

module.exports = {
  e2e: {
    username: env.E2E_USER,
    password: env.E2E_PASSWORD,
    database: env.E2E_APP,
    host: env.E2E_HOST,
    port: Number(env.E2E_PORT),
    dialect: 'mysql',
    dialectOptions: { ssl: { ca: env.E2E_CA ? fs.readFileSync(env.E2E_CA) : undefined } },
    define: { underscored: true },
    logging: (sql) => console.log(sql),
  },
};
