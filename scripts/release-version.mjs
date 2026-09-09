import { readFile, appendFile } from 'node:fs/promises';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { resolve } from 'node:path';

export const releaseRoot = fileURLToPath(new URL('../', import.meta.url));
export const services = ['home-agent', 'signaling-server', 'turn-server'];
export const versionPattern = /^v\d+\.\d+\.\d+(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?$/;

export async function buildVersion(env = process.env, sourceRoot = releaseRoot) {
  const versions = await Promise.all(services.map(async (service) => {
    const manifest = await readFile(resolve(sourceRoot, service, 'Cargo.toml'), 'utf8');
    const version = manifest.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
    if (!version) throw new Error('Missing package version: ' + service);
    return version;
  }));
  if (new Set(versions).size !== 1) throw new Error('All three Rust services must use the same release version');
  const base = 'v' + versions[0];
  if (!versionPattern.test(base)) throw new Error('Invalid Cargo package version: ' + versions[0]);
  if (env.GITHUB_REF_TYPE === 'tag') {
    if (!versionPattern.test(env.GITHUB_REF_NAME || '') || env.GITHUB_REF_NAME !== base) {
      throw new Error('Release tag must match all Cargo.toml versions: ' + base);
    }
    return base;
  }
  const revision = env.GITHUB_SHA ? env.GITHUB_SHA.slice(0, 8) : 'local';
  if (!/^[a-zA-Z0-9]+$/.test(revision)) throw new Error('Invalid source revision');
  return base + (base.includes('-') ? '.dev.' : '-dev.') + revision;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const version = await buildVersion();
  if (process.env.GITHUB_OUTPUT) await appendFile(process.env.GITHUB_OUTPUT, 'version=' + version + '\n');
  console.log(version);
}
