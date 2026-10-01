#!/usr/bin/env node
/** A stand-in for `launchctl` and `systemctl --user`, enough of each for `service.mjs`, for any number of jobs: it reads
 *  the definition the connector wrote, starts its program detached with its environment (as RunAtLoad / `start` would)
 *  under a small monitor of its own that records the program's pid and, when it ends, its exit status or signal; stops
 *  it with SIGTERM and waits; and answers `print` / `show` with what it knows, in the managers' own formats. Its state
 *  is a directory (`FAKE_MANAGER_DIR`): one subdirectory per job (`loaded`, `definition`, `pid`, `exit`, `runs`), and
 *  every call in `calls.jsonl`. `FAKE_MANAGER_FAIL=<verb>` makes that verb fail and change nothing — an unload the
 *  manager refuses, a start it will not do. A job's `force.json` overrides what it answers (`{restarting, runs, exit,
 *  startLimit}`): how a test shows `deriveStatus` a manager that is about to start a job again, or gave up.
 *
 *  Not modelled: KeepAlive / Restart= (a job that exits stays down) and the start limit. The real managers are
 *  exercised in CI (`test/service-integration.mjs`). */
import { spawn } from 'node:child_process';
import { appendFileSync, existsSync, mkdirSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import path from 'node:path';

const [kind, ...args] = process.argv.slice(2);
const dir = process.env.FAKE_MANAGER_DIR;
mkdirSync(dir, { recursive: true });
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
const alive = pid => { try { process.kill(pid, 0); return true; } catch { return false; } };

/** `monitor <job dir>`: run the job's program, and say how it ended. */
if (kind === 'monitor') {
  const [jobDir, spec] = [args[0], JSON.parse(readFileSync(path.join(args[0], 'spec.json'), 'utf8'))];
  const out = openSync(spec.log, 'a', 0o600);
  const child = spawn(spec.program[0], spec.program.slice(1), { stdio: ['ignore', out, out], env: { HOME: process.env.HOME, PATH: '/usr/bin:/bin', ...spec.environment } });
  child.on('error', error => { writeFileSync(path.join(jobDir, 'exit'), JSON.stringify({ code: error.code === 'ENOENT' ? 78 : 126, signal: null })); process.exit(0); });
  if (child.pid) writeFileSync(path.join(jobDir, 'pid'), String(child.pid));
  for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => child.kill(signal));
  child.on('exit', (code, signal) => { writeFileSync(path.join(jobDir, 'exit'), JSON.stringify({ code, signal })); process.exit(0); });
  await new Promise(() => {});
}

appendFileSync(path.join(dir, 'calls.jsonl'), JSON.stringify([kind, ...args]) + '\n');
const verb = kind === 'systemctl' ? args.filter(arg => arg !== '--user')[0] : args[0];
if (process.env.FAKE_MANAGER_FAIL === verb) { console.error(`fake ${kind}: ${verb} refused as asked`); process.exit(1); }

const jobDir = name => { const at = path.join(dir, name.replace(/[^\w.@-]/g, '_')); mkdirSync(at, { recursive: true }); return at; };
const read = (job, name) => { try { return readFileSync(path.join(jobDir(job), name), 'utf8').trim(); } catch { return null; } };
const write = (job, name, value) => writeFileSync(path.join(jobDir(job), name), String(value));
const pid = job => Number(read(job, 'pid')) || null;
const running = job => !!pid(job) && alive(pid(job));
const force = job => { try { return JSON.parse(read(job, 'force.json')) ?? {}; } catch { return {}; } };
const ended = job => { try { return JSON.parse(read(job, 'exit')); } catch { return null; } };

/** The definition `service.mjs` wrote: the program, its environment and its log. */
function definition(job) {
  const source = read(job, 'definition');
  const text = source && existsSync(source) ? readFileSync(source, 'utf8') : null;
  if (!text) return null;
  if (kind === 'launchctl') {
    const decode = value => value.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&amp;/g, '&');
    const strings = block => [...block.matchAll(/<string>([^<]*)<\/string>/g)].map(match => decode(match[1]));
    const program = strings(text.match(/<key>ProgramArguments<\/key>\s*<array>([\s\S]*?)<\/array>/)[1]);
    const pairs = [...text.match(/<key>EnvironmentVariables<\/key>\s*<dict>([\s\S]*?)<\/dict>/)[1].matchAll(/<key>([^<]*)<\/key>\s*<string>([^<]*)<\/string>/g)];
    return { program, environment: Object.fromEntries(pairs.map(([, name, value]) => [decode(name), decode(value)])), log: decode(text.match(/<key>StandardErrorPath<\/key>\s*<string>([^<]*)<\/string>/)[1]) };
  }
  // As systemd reads them: `$$` is a literal `$` in ExecStart= only; Environment= takes `$` as it is.
  const words = (line, exec) => [...line.matchAll(/"((?:[^"\\]|\\.)*)"/g)].map(match => { const value = match[1].replace(/\\(.)/g, '$1').replace(/%%/g, '%'); return exec ? value.replace(/\$\$/g, '$') : value; });
  const program = words(text.match(/^ExecStart=(.*)$/m)[1], true);
  const environment = Object.fromEntries(text.split('\n').filter(line => line.startsWith('Environment=')).map(line => { const [pair] = words(line.slice(12), false); const at = pair.indexOf('='); return [pair.slice(0, at), pair.slice(at + 1)]; }));
  // The journal, standing in: the job's output beside its state.
  return { program, environment, log: path.join(jobDir(job), 'journal.log') };
}

async function launch(job) {
  if (running(job)) return;
  const spec = definition(job);
  if (!spec) { console.error(`fake: no definition for ${job}`); process.exit(5); }
  write(job, 'spec.json', JSON.stringify(spec));
  rmSync(path.join(jobDir(job), 'pid'), { force: true });
  rmSync(path.join(jobDir(job), 'exit'), { force: true });
  write(job, 'runs', (Number(read(job, 'runs')) || 0) + 1);
  const monitor = spawn(process.execPath, [process.argv[1], 'monitor', jobDir(job)], { detached: true, stdio: 'ignore', env: { ...process.env } });
  write(job, 'monitor', monitor.pid);
  monitor.unref();
  for (let i = 0; i < 100 && !pid(job) && !ended(job); i++) await wait(20);
}

async function halt(job) {
  const current = pid(job);
  if (!current || !alive(current)) return;
  try { process.kill(current, 'SIGTERM'); } catch {}
  for (let i = 0; i < 250 && alive(current); i++) await wait(100);
  if (alive(current)) { try { process.kill(current, 'SIGKILL'); } catch {} }
  for (let i = 0; i < 50 && !ended(job); i++) await wait(20);
}

const target = args.at(-1);
if (kind === 'launchctl') {
  const label = target.split('/').at(-1);
  const loaded = () => read(label, 'loaded') === 'yes';
  if (verb === 'print') {
    if (!loaded()) { console.error(`Could not find service "${target}" in domain for port`); process.exit(113); }
    const forced = force(label), last = ended(label);
    const up = running(label) && !forced.restarting;
    const exit = forced.exit ?? (forced.restarting ? 1 : last ? (last.code ?? null) : null);
    console.log(`${target} = {\n\tactive count = ${up ? 1 : 0}\n\tstate = ${up ? 'running' : 'not running'}\n${up ? `\tpid = ${pid(label)}\n` : ''}\truns = ${forced.runs ?? (Number(read(label, 'runs')) || 0)}\n\tlast exit code = ${exit === null ? '(never exited)' : exit}\n${last?.signal ? `\tlast terminating signal = ${last.signal}\n` : ''}}`);
  } else if (verb === 'bootstrap') {
    const file = args[2], name = path.basename(file, '.plist');
    if (read(name, 'loaded') === 'yes') { console.error('Bootstrap failed: 5: Input/output error'); process.exit(5); }
    write(name, 'definition', file); write(name, 'loaded', 'yes'); write(name, 'runs', 0); await launch(name);
  } else if (verb === 'kickstart') { if (!loaded()) { console.error('Could not find service'); process.exit(113); } if (args.includes('-k')) await halt(label); await launch(label); }
  else if (verb === 'bootout') { if (!loaded()) process.exit(3); await halt(label); rmSync(path.join(jobDir(label), 'loaded'), { force: true }); }
  else { console.error('fake launchctl: unknown ' + verb); process.exit(64); }
} else {
  const unit = target;
  const source = name => path.join(process.env.XDG_CONFIG_HOME || path.join(process.env.HOME, '.config'), 'systemd', 'user', name);
  const known = () => !!read(unit, 'definition') && existsSync(read(unit, 'definition'));
  if (verb === 'show-environment') console.log('HOME=' + process.env.HOME);
  else if (verb === 'daemon-reload') {
    for (const name of ['sidevoice-core.service', 'sidevoice-connector.service']) {
      if (existsSync(source(name))) write(name, 'definition', source(name)); else rmSync(path.join(jobDir(name), 'definition'), { force: true });
    }
  } else if (verb === 'enable' || verb === 'reset-failed') { if (verb === 'reset-failed') rmSync(path.join(jobDir(unit), 'force.json'), { force: true }); }
  else if (verb === 'start') { if (!known()) { console.error(`Unit ${unit} not found.`); process.exit(5); } await launch(unit); }
  else if (verb === 'stop') await halt(unit);
  else if (verb === 'restart') { if (!known()) { console.error(`Unit ${unit} not found.`); process.exit(5); } await halt(unit); await launch(unit); }
  else if (verb === 'disable') { if (args.includes('--now')) await halt(unit); }
  else if (verb === 'show') {
    const forced = force(unit), last = ended(unit), up = running(unit) && !forced.restarting;
    console.log([`LoadState=${known() ? 'loaded' : 'not-found'}`,
      `ActiveState=${up ? 'active' : forced.restarting ? 'activating' : forced.startLimit || (last && (last.code || last.signal)) ? 'failed' : 'inactive'}`,
      `SubState=${up ? 'running' : forced.restarting ? 'auto-restart' : forced.startLimit ? 'failed' : 'dead'}`,
      `Result=${forced.startLimit ? 'start-limit-hit' : last?.signal ? 'signal' : last?.code ? 'exit-code' : 'success'}`,
      `ExecMainStatus=${forced.exit ?? last?.code ?? 0}`, `ExecMainPID=${up ? pid(unit) : 0}`, `NRestarts=${forced.runs ?? Math.max(0, (Number(read(unit, 'runs')) || 1) - 1)}`].join('\n'));
  } else { console.error('fake systemctl: unknown ' + verb); process.exit(64); }
}
