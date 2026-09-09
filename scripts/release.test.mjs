import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, mkdir, readFile, readdir, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import test from 'node:test';
import { buildVersion, services } from './release-version.mjs';
import { deploymentFiles, packageRelease, sha256, targets, verifyPackages } from './package-release.mjs';
import { prepareRelease } from './prepare-release.mjs';

const execute = promisify(execFile);
const tar = process.platform === 'win32' ? 'tar.exe' : 'tar';

async function fixture(run) {
  const temporaryRoot = await realpath(tmpdir());
  const directory = await mkdtemp(join(temporaryRoot, 'ai-remote-release-test-'));
  try { return await run(directory); }
  finally {
    assert.equal(dirname(directory), temporaryRoot);
    assert.ok(basename(directory).startsWith('ai-remote-release-test-'));
    await rm(directory, { recursive: true, force: true });
  }
}

async function manifests(directory, versions) {
  for (let index = 0; index < services.length; index++) {
    await mkdir(join(directory, services[index]), { recursive: true });
    await writeFile(join(directory, services[index], 'Cargo.toml'), `[package]\nname = "${services[index]}"\nversion = "${versions[index]}"\n`);
  }
}

test('release tags must match every service, while branch builds include their revision', () => fixture(async (directory) => {
  await manifests(directory, ['1.2.3', '1.2.3', '1.2.3']);
  assert.equal(await buildVersion({ GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: 'v1.2.3' }, directory), 'v1.2.3');
  assert.equal(await buildVersion({ GITHUB_SHA: '123456789abcdef' }, directory), 'v1.2.3-dev.12345678');
  await assert.rejects(buildVersion({ GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: 'v1.2.4' }, directory), /tag must match/);
  await manifests(directory, ['1.2.3', '1.2.4', '1.2.3']);
  await assert.rejects(buildVersion({}, directory), /same release version/);
  await manifests(directory, ['1.2.3-rc.1', '1.2.3-rc.1', '1.2.3-rc.1']);
  assert.equal(await buildVersion({}, directory), 'v1.2.3-rc.1.dev.local');
}));

test('native packages include the runnable layout and exclude local deployment secrets', () => fixture(async (directory) => {
  const target = process.platform === 'win32' ? 'x86_64-pc-windows-msvc' : process.platform === 'darwin' ? 'x86_64-apple-darwin' : 'x86_64-unknown-linux-gnu';
  const suffix = process.platform === 'win32' ? '.exe' : '';
  const binaries = join(directory, 'build');
  await mkdir(binaries);
  for (const service of services) await writeFile(join(binaries, service + suffix), service);
  for (const folder of ['frontend/dist', 'deploy', 'scripts']) await mkdir(join(directory, folder), { recursive: true });
  await writeFile(join(directory, 'frontend/dist/index.html'), '<div id="root"></div>');
  for (const file of deploymentFiles) await writeFile(join(directory, 'deploy', file), '# example\n');
  for (const file of ['home-agent.env', 'credentials.json', 'turn.env.backup', 'private.pem']) await writeFile(join(directory, 'deploy', file), 'must not be packaged');
  for (const file of ['run-home-agent.ps1', 'install-home-agent-task.ps1']) await writeFile(join(directory, 'scripts', file), '# runner\n');
  await writeFile(join(directory, 'README.md'), 'README');
  const outDir = join(directory, 'output');
  const { archive, digest } = await packageRelease({ target, version: 'v1.2.3', binDir: binaries, outDir, sourceRoot: directory });
  assert.equal(await sha256(archive), digest);
  assert.equal((await readFile(archive + '.sha256', 'utf8')).trim(), digest + '  ' + basename(archive));
  const extracted = join(directory, 'extracted');
  await mkdir(extracted);
  await execute(tar, ['-xf', archive, '-C', extracted]);
  const payload = join(extracted, 'ai-remote');
  assert.deepEqual((await readdir(join(payload, 'deploy'))).sort(), [...deploymentFiles].sort());
  assert.deepEqual((await readdir(join(payload, 'bin'))).sort(), services.map((service) => service + suffix).sort());
  assert.equal((await readFile(join(payload, 'VERSION'), 'utf8')).trim(), 'v1.2.3');
  assert.equal(JSON.parse(await readFile(join(payload, 'manifest.json'), 'utf8')).target, target);
  assert.match(await readFile(join(payload, 'frontend/index.html'), 'utf8'), /id="root"/);
  await assert.rejects(packageRelease({ target, version: '../invalid', binDir: binaries, outDir, sourceRoot: directory }), /Invalid release version/);
  await assert.rejects(packageRelease({ target: 'unknown', version: 'v1.2.3' }), /Unsupported release target/);
}));

test('publishing requires all six packages and valid checksums, including installer digests', () => fixture(async (directory) => {
  const archives = [];
  for (const [target, platform] of Object.entries(targets)) {
    const archive = join(directory, 'ai-remote-' + target + platform.extension);
    await writeFile(archive, target);
    await writeFile(archive + '.sha256', await sha256(archive) + '  ' + basename(archive) + '\n');
    archives.push(archive);
  }
  for (const installer of ['install.sh', 'install.ps1']) await writeFile(join(directory, installer), installer);
  const files = await prepareRelease({ directory, repository: 'sxhxliang/ai-remote', version: 'v1.2.3', sourceRoot: directory });
  assert.equal(files.length, 8);
  assert.equal((await readFile(join(directory, 'SHA256SUMS'), 'utf8')).trim().split('\n').length, 8);
  assert.match(await readFile(join(directory, 'release-notes.md'), 'utf8'), /releases\/download\/v1\.2\.3\/install\.sh/);
  await writeFile(archives[0], 'corrupted download');
  await assert.rejects(verifyPackages(directory), /Invalid checksum/);
  await rm(archives[1]);
  await assert.rejects(verifyPackages(directory), /Invalid checksum/);
  await writeFile(archives[0] + '.sha256', await sha256(archives[0]) + '  ' + basename(archives[0]) + '\n');
  await assert.rejects(verifyPackages(directory), { code: 'ENOENT' });
}));
