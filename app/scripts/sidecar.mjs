// Before `tauri build`: builds the web UI, then the release `ottie` binary
// that embeds it, and puts that binary where Tauri's bundler looks for the
// sidecar (`externalBin` in tauri.bundle.json): binaries/ottie-<target>.
import { execFileSync, execSync } from 'node:child_process';
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const app = dirname(dirname(fileURLToPath(import.meta.url)));
const root = dirname(app);
// Through a shell: on Windows, pnpm is a .cmd script.
const run = (command) => execSync(command, { cwd: root, stdio: 'inherit' });

run('pnpm --filter ui build');
// --locked: the release uses Cargo.lock's versions, as the Dockerfile does.
run('cargo build --release --locked -p ottie');

const target = /^host: (\S+)$/m.exec(execFileSync('rustc', ['-vV'], { encoding: 'utf8' }))[1];
const exe = process.platform === 'win32' ? '.exe' : '';
mkdirSync(join(app, 'binaries'), { recursive: true });
copyFileSync(
	join(root, 'target', 'release', `ottie${exe}`),
	join(app, 'binaries', `ottie-${target}${exe}`)
);
