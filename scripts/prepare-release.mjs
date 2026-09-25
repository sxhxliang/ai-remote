import { writeFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { sha256, verifyPackages } from './package-release.mjs';
import { releaseRoot, versionPattern } from './release-version.mjs';

export async function prepareRelease({ directory = resolve(releaseRoot, '.artifacts'), repository, version, sourceRoot = releaseRoot }) {
  if (!/^[A-Za-z0-9][A-Za-z0-9_.-]*\/[A-Za-z0-9][A-Za-z0-9_.-]*$/.test(repository || '')) throw new Error('Invalid release repository');
  if (!versionPattern.test(version || '')) throw new Error('Invalid release tag');
  const files = await verifyPackages(directory);
  for (const installer of ['install.sh', 'install.ps1']) {
    const archive = resolve(sourceRoot, installer);
    files.push({ archive, digest: await sha256(archive) });
  }
  await writeFile(resolve(directory, 'SHA256SUMS'), files.map(({ archive, digest }) => digest + '  ' + basename(archive)).join('\n') + '\n');
  const url = `https://github.com/${repository}/releases/download/${version}`;
  const notes = `AI Remote ${version}：通过浏览器的 WebRTC DataChannel 访问家里的模型服务。

本版新增 OpenAI 兼容聊天接口：自动读取 /v1/models，使用流式 /v1/chat/completions 聊天。可直接使用 Ollama 已安装的模型，也可在家庭 Agent 配置 OPENAI_BASE 和 OPENAI_API_KEY 连接其他兼容服务。Ollama 原生接口继续可用。

提供 Linux、macOS、Windows 的 x64 / ARM64 安装包，包含家庭 Agent、信令服务、TURN 服务、Web UI 和配置模板。使用预编译包无需 Rust 或 Node.js。

Linux / macOS：

\`\`\`sh
curl -fsSL ${url}/install.sh | AI_REMOTE_VERSION=${version} sh
\`\`\`

Windows PowerShell：

\`\`\`powershell
& ([scriptblock]::Create((Invoke-RestMethod '${url}/install.ps1'))) -Version ${version}
\`\`\`

安装脚本自动选择架构、验证 SHA-256，并保留已有配置。安装后需填写信令和 TURN 凭据，部署方式见 [README](https://github.com/${repository}/blob/${version}/README.md)。

Linux 需要 glibc 2.35+；macOS 需要 11+；Windows 需要 Windows 10/11 或 Server 2019+。每个安装包附带独立的 .sha256 文件，SHA256SUMS 同时覆盖六个安装包和两个安装脚本。
`;
  await writeFile(resolve(directory, 'release-notes.md'), notes);
  return files;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const files = await prepareRelease({ repository: process.env.GITHUB_REPOSITORY, version: process.env.RELEASE_TAG });
  console.log('Prepared release notes and SHA256SUMS for ' + files.length + ' assets.');
}
