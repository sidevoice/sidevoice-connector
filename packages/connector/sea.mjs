/** Static entry for Node 22's CommonJS single-executable application. */
import { main } from './cli.mjs';

async function run() {
  const result = await main(process.argv.slice(2));
  if (typeof result === 'number') process.exitCode = result;
}

run().catch(error => {
  console.error(error?.message || error);
  process.exitCode = 1;
});
