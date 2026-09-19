import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { access, chmod, cp, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { basename, dirname, join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { spawn } from 'node:child_process';
import { parseArgs } from 'node:util';
import { pathToFileURL } from 'node:url';
import { releaseRoot, services, versionPattern } from './release-version.mjs';

export const targets = {
  'x86_64-unknown-linux-gnu': { os: 'linux', arch: 'x64', extension: '.tar.gz' },
  'aarch64-unknown-linux-gnu': { os: 'linux', arch: 'arm64', extension: '.tar.gz' },
  'x86_64-apple-darwin': { os: 'macos', arch: 'x64', extension: '.tar.gz' },
  'aarch64-apple-darwin': { os: 'macos', arch: 'arm64', extension: '.tar.gz' },
  'x86_64-pc-windows-msvc': { os: 'windows', arch: 'x64', extension: '.zip' },
  'aarch64-pc-windows-msvc': { os: 'windows', arch: 'arm64', extension: '.zip' },
};

export const deploymentFiles = [
  'home-agent.env.example', 'signaling.env.example', 'turn.env.example',
  'ollama-link-home-agent.service', 'ollama-link-signaling.service', 'ollama-link-turn.service',
  'deploy.sh',
];

// Git Bash also supplies a tar.exe, but it treats Windows drive letters as
// remote hosts and cannot create these ZIP packages. Use Windows' bsdtar.
export const tarCommand = process.platform === 'win32'
  ? join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe') : 'tar';

export async function sha256(path) {
  const hash = createHash('sha256');
  for await (const bytes of createReadStream(path)) hash.update(bytes);
  return hash.digest('hex');
}

async function run(command, args, cwd) {
  await new Promise((done, reject) => {
    const child = spawn(command, args, { cwd, stdio: 'inherit', windowsHide: true });
    child.once('error', reject);
    child.once('exit', (code) => code === 0 ? done() : reject(new Error(command + ' exited with ' + code)));
  });
}

export async function packageRelease({ target, version, binDir, outDir = resolve(releaseRoot, '.artifacts'), sourceRoot = releaseRoot }) {
  const platform = targets[target];
  if (!platform) throw new Error('Unsupported release target: ' + target);
  if (!versionPattern.test(version || '')) throw new Error('Invalid release version');
  if (platform.os === 'windows' && process.platform !== 'win32') throw new Error('Build Windows ZIP packages on Windows');
  const temporaryRoot = await realpath(tmpdir());
  const staging = await mkdtemp(join(temporaryRoot, 'ai-remote-package-'));
  try {
    const payload = join(staging, 'ai-remote');
    await mkdir(join(payload, 'bin'), { recursive: true });
    const suffix = platform.os === 'windows' ? '.exe' : '';
    for (const service of services) {
      const source = binDir ? resolve(binDir, service + suffix) : resolve(sourceRoot, service, 'target', target, 'release', service + suffix);
      await access(source);
      const destination = join(payload, 'bin', service + suffix);
      await cp(source, destination);
      await chmod(destination, 0o755);
    }
    await access(resolve(sourceRoot, 'frontend/dist/index.html'));
    await cp(resolve(sourceRoot, 'frontend/dist'), join(payload, 'frontend'), { recursive: true });
    await mkdir(join(payload, 'deploy'));
    for (const file of deploymentFiles) {
      await cp(resolve(sourceRoot, 'deploy', file), join(payload, 'deploy', file));
    }
    await mkdir(join(payload, 'scripts'));
    for (const script of ['run-home-agent.ps1', 'install-home-agent-task.ps1']) {
      await cp(resolve(sourceRoot, 'scripts', script), join(payload, 'scripts', script));
    }
    await cp(resolve(sourceRoot, 'README.md'), join(payload, 'README.md'));
    await writeFile(join(payload, 'VERSION'), version + '\n');
    await writeFile(join(payload, 'manifest.json'), JSON.stringify({ version, target, ...platform, services }, null, 2) + '\n');
    await mkdir(outDir, { recursive: true });
    const archive = resolve(outDir, 'ai-remote-' + target + platform.extension);
    const args = platform.os === 'windows' ? ['-a', '-cf', archive, 'ai-remote'] : ['-czf', archive, 'ai-remote'];
    await run(tarCommand, args, staging);
    const digest = await sha256(archive);
    await writeFile(archive + '.sha256', digest + '  ' + basename(archive) + '\n');
    return { archive, digest };
  } finally {
    if (dirname(staging) !== temporaryRoot || !basename(staging).startsWith('ai-remote-package-')) throw new Error('Unsafe package cleanup path');
    await rm(staging, { recursive: true, force: true });
  }
}

export async function verifyPackages(directory) {
  const packages = [];
  for (const [target, platform] of Object.entries(targets)) {
    const archive = resolve(directory, 'ai-remote-' + target + platform.extension);
    const expected = (await readFile(archive + '.sha256', 'utf8')).trim();
    const digest = await sha256(archive);
    if (expected !== digest + '  ' + basename(archive)) throw new Error('Invalid checksum: ' + basename(archive));
    packages.push({ archive, digest });
  }
  return packages;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const { values } = parseArgs({ options: { target: { type: 'string' }, version: { type: 'string' }, 'bin-dir': { type: 'string' }, 'out-dir': { type: 'string' }, verify: { type: 'string' } } });
  if (values.verify) { await verifyPackages(values.verify); console.log('All six release packages verified.'); }
  else {
    const result = await packageRelease({ target: values.target, version: values.version, binDir: values['bin-dir'], outDir: values['out-dir'] });
    console.log(result.archive);
  }
}
