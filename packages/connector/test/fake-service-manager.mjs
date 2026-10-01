#!/usr/bin/env node
/** A stand-in for `launchctl` and `systemctl --user`, enough of each for `service.mjs`: it reads the definition
 *  the connector wrote, starts its program detached with its environment (as RunAtLoad / `start` would), stops it
 *  with SIGTERM and waits, and answers `print` / `show` with what it knows. Its state is a directory
 *  (`FAKE_MANAGER_DIR`): `loaded`, `pid`, and every call in `calls.jsonl`. `FAKE_MANAGER_FAIL=<verb>` makes that
 *  verb fail and change nothing — an unload the manager refuses, a start it will not do.
 *
 *  Not modelled: KeepAlive / Restart=on-failure (a supervisor that exits stays down) and the start limit. The real
 *  managers are exercised in CI (`test/service-integration.mjs`). */
import { spawn } from 'node:child_process';
import { appendFileSync, existsSync, mkdirSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import path from 'node:path';

const [kind, ...args] = process.argv.slice(2);
const dir = process.env.FAKE_MANAGER_DIR;
mkdirSync(dir, { recursive: true });
appendFileSync(path.join(dir, 'calls.jsonl'), JSON.stringify([kind, ...args]) + '\n');
const verb = kind === 'systemctl' ? args.filter(arg => arg !== '--user')[0] : args[0];
if (process.env.FAKE_MANAGER_FAIL === verb) { console.error(`fake ${kind}: ${verb} refused as asked`); process.exit(1); }

const file = name => path.join(dir, name);
const read = name => { try { return readFileSync(file(name), 'utf8').trim(); } catch { return null; } };
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };
const pid = () => Number(read('pid')) || null;
const running = () => !!pid() && alive(pid());
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

/** The definition `service.mjs` wrote: the program, its environment and the log. */
function definition() {
  const source = read('definition');
  const text = source && existsSync(source) ? readFileSync(source, 'utf8') : null;
  if (!text) return null;
  if (kind === 'launchctl') {
    const strings = block => [...block.matchAll(/<string>([^<]*)<\/string>/g)].map(match => decode(match[1]));
    const decode = value => value.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');
    const program = strings(text.match(/<key>ProgramArguments<\/key>\s*<array>([\s\S]*?)<\/array>/)[1]);
    const pairs = [...text.match(/<key>EnvironmentVariables<\/key>\s*<dict>([\s\S]*?)<\/dict>/)[1].matchAll(/<key>([^<]*)<\/key>\s*<string>([^<]*)<\/string>/g)];
    return { program, environment: Object.fromEntries(pairs.map(([, name, value]) => [decode(name), decode(value)])), log: decode(text.match(/<key>StandardErrorPath<\/key>\s*<string>([^<]*)<\/string>/)[1]) };
  }
  const words = line => [...line.matchAll(/"((?:[^"\\]|\\.)*)"/g)].map(match => match[1].replace(/\\(.)/g, '$1').replace(/%%/g, '%').replace(/\$\$/g, '$'));
  const program = words(text.match(/^ExecStart=(.*)$/m)[1]);
  const environment = Object.fromEntries(text.split('\n').filter(line => line.startsWith('Environment=')).map(line => { const [pair] = words(line.slice(12)); const at = pair.indexOf('='); return [pair.slice(0, at), pair.slice(at + 1)]; }));
  return { program, environment, log: text.match(/^StandardError=append:(.*)$/m)[1] };
}

function launch() {
  if (running()) return;
  const spec = definition();
  if (!spec) { console.error('fake: no definition'); process.exit(5); }
  const out = openSync(spec.log, 'a', 0o600);
  const env = { HOME: process.env.HOME, PATH: '/usr/bin:/bin', ...spec.environment };
  const child = spawn(spec.program[0], spec.program.slice(1), { detached: true, stdio: ['ignore', out, out], env });
  writeFileSync(file('pid'), String(child.pid));
  child.unref();
}

async function halt() {
  const current = pid();
  if (!current || !alive(current)) return;
  try { process.kill(current, 'SIGTERM'); } catch {}
  for (let i = 0; i < 250 && alive(current); i++) await wait(100);
  if (alive(current)) { try { process.kill(current, 'SIGKILL'); } catch {} }
}

const target = args.at(-1);
if (kind === 'launchctl') {
  const loaded = () => read('loaded') === 'yes';
  if (verb === 'print') { if (!loaded()) { console.error(`Could not find service "${target}"`); process.exit(113); } console.log(`${target} = {\n\tstate = ${running() ? 'running' : 'not running'}\n\tpid = ${running() ? pid() : ''}\n\tlast exit code = 0\n}`); }
  else if (verb === 'bootstrap') { if (loaded()) { console.error('Bootstrap failed: 5: Input/output error'); process.exit(5); } writeFileSync(file('definition'), args[2]); writeFileSync(file('loaded'), 'yes'); launch(); }
  else if (verb === 'kickstart') { if (!loaded()) { console.error('Could not find service'); process.exit(113); } if (args.includes('-k')) await halt(); launch(); }
  else if (verb === 'bootout') { if (!loaded()) process.exit(3); await halt(); rmSync(file('loaded'), { force: true }); }
  else { console.error('fake launchctl: unknown ' + verb); process.exit(64); }
} else {
  const unit = () => read('definition');
  if (verb === 'show-environment') console.log('HOME=' + process.env.HOME);
  else if (verb === 'daemon-reload') { const source = path.join(process.env.XDG_CONFIG_HOME || path.join(process.env.HOME, '.config'), 'systemd', 'user', 'sidevoice-node.service'); if (existsSync(source)) writeFileSync(file('definition'), source); else rmSync(file('definition'), { force: true }); }
  else if (verb === 'enable' || verb === 'reset-failed') {}
  else if (verb === 'start') { if (!unit()) { console.error('Unit sidevoice-node.service not found.'); process.exit(5); } launch(); }
  else if (verb === 'stop') await halt();
  else if (verb === 'restart') { await halt(); launch(); }
  else if (verb === 'disable') { if (args.includes('--now')) await halt(); }
  else if (verb === 'show') {
    const loaded = !!unit() && existsSync(unit());
    console.log(`LoadState=${loaded ? 'loaded' : 'not-found'}\nActiveState=${running() ? 'active' : 'inactive'}\nResult=success\nExecMainStatus=0`);
  } else { console.error('fake systemctl: unknown ' + verb); process.exit(64); }
}
