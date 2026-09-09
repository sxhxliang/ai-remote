import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { chmod, cp, mkdir, mkdtemp, readFile, readdir, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, delimiter, dirname, join, resolve, sep } from 'node:path';
import { parseArgs, promisify } from 'node:util';
import { packageRelease, sha256, targets } from './package-release.mjs';
import { releaseRoot, versionPattern } from './release-version.mjs';

const execute = promisify(execFile);
const { values } = parseArgs({ options: { target: { type: 'string' }, version: { type: 'string' }, 'artifact-dir': { type: 'string' } } });
const { target, version } = values;
const platform = targets[target];
if (!platform || !versionPattern.test(version || '')) throw new Error('Provide a valid --target and --version');
const windows = process.platform === 'win32';
if (platform.os !== ({ win32: 'windows', darwin: 'macos', linux: 'linux' })[process.platform]) throw new Error('Run installer tests on the release target OS');
const artifactDir = resolve(values['artifact-dir'] || join(releaseRoot, '.artifacts'));
const asset = 'ai-remote-' + target + platform.extension;
const temporaryRoot = await realpath(tmpdir());
const temporary = await mkdtemp(join(temporaryRoot, 'ai-remote-installer-test-'));
const downloads = join(temporary, 'downloads');
const installRoot = join(temporary, 'application with spaces');
const binDirectory = windows ? join(installRoot, 'bin') : join(temporary, 'commands with spaces');
const fakeBin = join(temporary, 'fake-bin');
const manifest = await readFile(join(releaseRoot, 'home-agent/Cargo.toml'), 'utf8');
const binaryVersion = manifest.match(/^version\s*=\s*"([^"]+)"/m)[1];
let downloadVersion = version;

async function install({ requested = 'latest', powershell = 'powershell.exe' } = {}) {
  const options = { cwd: temporary, windowsHide: true, timeout: 120000, maxBuffer: 1024 * 1024 };
  if (windows) {
    // Each PowerShell edition must resolve its own built-in modules. A Node
    // process launched by pwsh otherwise passes PS7's modules to PowerShell 5.1.
    const env = { ...process.env };
    for (const key of Object.keys(env)) if (key.toLowerCase() === 'psmodulepath') delete env[key];
    return execute(powershell, [
      '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', join(releaseRoot, 'scripts/test-installers.ps1'),
      '-Installer', join(releaseRoot, 'install.ps1'), '-FixtureDirectory', downloads,
      '-ReleaseVersion', downloadVersion, '-ArchiveName', asset, '-InstallDir', installRoot + sep,
      '-BinaryVersion', binaryVersion, '-Version', requested,
    ], { ...options, env });
  }
  const result = await execute('sh', [join(releaseRoot, 'install.sh')], {
    ...options,
    env: { ...process.env, PATH: fakeBin + delimiter + process.env.PATH, AI_REMOTE_REPO: 'fixture/ai-remote',
      AI_REMOTE_VERSION: requested, AI_REMOTE_INSTALL_DIR: installRoot, AI_REMOTE_BIN_DIR: binDirectory,
      MOCK_RELEASE_DIR: downloads, MOCK_RELEASE_VERSION: downloadVersion, MOCK_RELEASE_ASSET: asset },
  });
  for (const [command, service] of Object.entries({ agent: 'home-agent', signaling: 'signaling-server', turn: 'turn-server' })) {
    const { stdout } = await execute(join(binDirectory, 'ai-remote-' + command), ['--version'], options);
    assert.equal(stdout.trim(), service + ' ' + binaryVersion);
  }
  return result;
}

async function currentVersion() {
  if (windows) return JSON.parse(await readFile(join(installRoot, 'current.json'), 'utf8')).version;
  return (await readFile(join(installRoot, 'current/VERSION'), 'utf8')).trim();
}

async function expectFailure(pattern) {
  await assert.rejects(install(), (error) => {
    assert.match(String(error.stderr || '') + String(error.stdout || '') + error.message, pattern);
    return true;
  });
}

try {
  await mkdir(downloads);
  await mkdir(fakeBin);
  for (const name of [asset, asset + '.sha256']) await cp(join(artifactDir, name), join(downloads, name));
  if (!windows) {
    const curl = `#!/usr/bin/env node
const fs = require('node:fs');
const path = require('node:path');
const args = process.argv.slice(2);
const url = args.at(-1);
const version = process.env.MOCK_RELEASE_VERSION;
const asset = process.env.MOCK_RELEASE_ASSET;
if (args[args.indexOf('--proto') + 1] !== '=https') throw new Error('HTTPS is required');
if (url === 'https://github.com/fixture/ai-remote/releases/latest') {
  process.stdout.write('https://github.com/fixture/ai-remote/releases/tag/' + version);
} else {
  const name = url.slice(url.lastIndexOf('/') + 1);
  if (![asset, asset + '.sha256'].includes(name) || url !== 'https://github.com/fixture/ai-remote/releases/download/' + version + '/' + name) throw new Error('Unexpected URL: ' + url);
  const output = args[args.indexOf('--output') + 1];
  fs.copyFileSync(path.join(process.env.MOCK_RELEASE_DIR, name), output);
}
`;
    await writeFile(join(fakeBin, 'curl'), curl);
    await chmod(join(fakeBin, 'curl'), 0o755);
  }

  await install();
  assert.equal(await currentVersion(), version);
  console.log('PASS latest installation and all three native commands');
  const configurations = {};
  for (const name of ['home-agent', 'signaling', 'turn']) {
    const path = join(installRoot, 'config', name + '.env');
    configurations[path] = await readFile(path, 'utf8') + '\n# Keep my existing settings.\n';
    await writeFile(path, configurations[path]);
  }
  const checkPreserved = async () => {
    for (const [path, contents] of Object.entries(configurations)) assert.equal(await readFile(path, 'utf8'), contents);
  };
  await install({ requested: version, powershell: 'pwsh.exe' });
  await checkPreserved();
  assert.equal((await readdir(join(installRoot, 'versions'))).length, 1);
  console.log('PASS explicit version and repeated installation preserve configuration');

  const originalDirectory = join(installRoot, 'versions', version + '-' + target);
  const upgraded = version + (version.includes('-') ? '.' : '-') + 'installer-upgrade';
  await packageRelease({ target, version: upgraded, binDir: join(originalDirectory, 'bin'), outDir: downloads });
  downloadVersion = upgraded;
  await install();
  await checkPreserved();
  assert.equal(await currentVersion(), upgraded);
  assert.equal((await readdir(join(installRoot, 'versions'))).length, 2);
  console.log('PASS upgrade switches commands and preserves configuration and previous version');

  await writeFile(join(downloads, asset + '.sha256'), '0'.repeat(64) + '  ' + asset + '\n');
  await expectFailure(/SHA-256 verification failed/);
  assert.equal(await currentVersion(), upgraded);
  await checkPreserved();
  console.log('PASS checksum mismatch leaves the active installation unchanged');

  const unsafe = join(temporary, 'unsafe-archive');
  await mkdir(unsafe);
  await writeFile(join(unsafe, 'outside.txt'), 'must not be extracted');
  const archive = join(downloads, asset);
  await execute(windows ? 'tar.exe' : 'tar', windows ? ['-a', '-cf', archive, 'outside.txt'] : ['-czf', archive, 'outside.txt'], { cwd: unsafe, windowsHide: true });
  await writeFile(archive + '.sha256', await sha256(archive) + '  ' + asset + '\n');
  await expectFailure(/Archive contains an (unsafe|unexpected) path/);
  assert.equal(await currentVersion(), upgraded);
  await checkPreserved();
  assert.ok(!(await readdir(installRoot)).some((name) => name.startsWith('.install-') || name.startsWith('.install.') || name === 'outside.txt'));
  console.log('PASS archive path rejection and staging cleanup');
  console.log('All installer checks passed for ' + target + (windows ? ' (PowerShell 5.1 and 7).' : '.'));
} finally {
  if (dirname(temporary) !== temporaryRoot || !basename(temporary).startsWith('ai-remote-installer-test-')) throw new Error('Unsafe installer-test cleanup path');
  await rm(temporary, { recursive: true, force: true });
}
