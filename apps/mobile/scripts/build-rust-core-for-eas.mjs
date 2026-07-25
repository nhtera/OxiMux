// Builds the `oximux-core` turbo module's native half on an EAS worker.
//
// Why this exists: everything the CocoaPods spec vendors — the xcframework, the
// generated C++/TS/ObjC bindings — is produced from `crates/mobile-core` and is
// deliberately untracked (see modules/oximux-core/.gitignore), so a fresh clone
// has the Rust source but none of the compiled artifacts. Locally that is what
// `npm run bindings` does; on EAS nobody runs it, so `pod install` would fail on
// a missing `OximuxCoreFramework.xcframework`. EAS calls `eas-build-post-install`
// after installing node_modules and before `expo prebuild` + `pod install`, which
// is exactly the window this needs.
//
// The uploaded archive is the whole git repository (eas-cli tars from
// `git rev-parse --show-toplevel`), so the cargo workspace four levels up is
// present and `ubrn.config.yaml`'s relative `rust.directory` resolves normally.

import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const mobileRoot = join(fileURLToPath(new URL('.', import.meta.url)), '..');
const coreModule = join(mobileRoot, 'modules', 'oximux-core');

// rustup drops its shims here; a freshly installed toolchain is not on PATH for
// the current process, so every later cargo/rustup call needs this prepended.
const cargoBin = join(homedir(), '.cargo', 'bin');
const env = { ...process.env, PATH: `${cargoBin}:${process.env.PATH}` };

function run(command, args, cwd = mobileRoot) {
  console.log(`\n▸ ${command} ${args.join(' ')}`);
  execFileSync(command, args, { cwd, env, stdio: 'inherit' });
}

function has(command) {
  try {
    execFileSync('sh', ['-c', `command -v ${command}`], { env, stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

const platform = process.env.EAS_BUILD_PLATFORM;
if (platform !== 'ios' && platform !== 'android') {
  console.log(`Skipping the native core build — unrecognised platform ${platform ?? '(unset)'}.`);
  process.exit(0);
}

// The image may ship without Rust. `--default-toolchain none` defers the version
// choice to the workspace's rust-toolchain.toml, so the worker builds with the
// same pinned compiler as a developer's machine instead of whatever is newest.
if (!has('rustup')) {
  console.log('\n▸ installing rustup (image has no Rust toolchain)');
  execFileSync(
    'sh',
    ['-c', 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none'],
    { stdio: 'inherit' }
  );
}

// Device only. A simulator slice would double the cargo time for a slice that an
// internal-distribution build can never run; add `aarch64-apple-ios-sim` back
// only if a profile sets `ios.simulator`.
const targets =
  platform === 'ios' ? ['aarch64-apple-ios'] : ['aarch64-linux-android', 'x86_64-linux-android'];
run('rustup', ['target', 'add', ...targets]);

// `npm install` at the app root symlinks the `file:` dependency without touching
// its devDependencies, so ubrn and bob — both needed below — are absent until
// this runs. `--include=dev` is explicit because EAS builds with NODE_ENV set.
if (!existsSync(join(coreModule, 'node_modules', '.bin', 'ubrn'))) {
  run('npm', ['ci', '--include=dev'], coreModule);
}

const buildArgs =
  platform === 'ios'
    ? ['ubrn', 'build', 'ios', '--release', '--no-sim', '--and-generate']
    : ['ubrn', 'build', 'android', '--release', '--and-generate'];
run('npx', buildArgs, coreModule);
run('npx', ['bob', 'build'], coreModule);

console.log('\n✓ native core ready for pod install');
