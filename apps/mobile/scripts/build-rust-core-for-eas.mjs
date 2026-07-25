// Builds the `oximux-core` turbo module's native half on an EAS worker.
//
// Why this exists: everything the CocoaPods spec vendors — the xcframework, the
// generated C++/TS/ObjC bindings — is produced from `crates/mobile-core` and is
// deliberately untracked (see modules/oximux-core/.gitignore), so a fresh clone
// has the Rust source but none of the compiled artifacts. Locally that is what
// `npm run bindings` does; on EAS nobody runs it.
//
// Why it runs at PRE-install rather than post: `oximux-core` is a `file:`
// dependency whose own `prepare` script is `bob build`, and npm runs that during
// the app's install. With no generated `src/index.tsx` yet, bob compiles zero
// files, writes no `lib/module/index.js`, and npm fails the whole install phase
// on "main field points to a non-existent file". The bindings therefore have to
// exist before npm ever looks at the module.
//
// That ordering means bootstrapping by hand: at pre-install nothing is installed
// anywhere, so this installs the module's own dependencies first (its lockfile is
// tracked) with scripts off — otherwise its `prepare` fails for the same reason
// the app's install would.
//
// The uploaded archive is the whole git repository (eas-cli tars from
// `git rev-parse --show-toplevel`), so the cargo workspace four levels up is
// present and `ubrn.config.yaml`'s relative `rust.directory` resolves normally.

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const mobileRoot = join(fileURLToPath(new URL('.', import.meta.url)), '..');
const coreModule = join(mobileRoot, 'modules', 'oximux-core');
const repoRoot = join(mobileRoot, '..', '..');

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

// The image may ship without Rust, or with a different default. Read the pinned
// channel rather than hardcoding it, so the worker and a developer's machine can
// never drift apart.
const channel = readFileSync(join(repoRoot, 'rust-toolchain.toml'), 'utf8').match(
  /channel\s*=\s*"([^"]+)"/
)?.[1];
if (!channel) throw new Error('No channel found in rust-toolchain.toml');

if (!has('rustup')) {
  console.log('\n▸ installing rustup (image has no Rust toolchain)');
  execFileSync(
    'sh',
    ['-c', 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none'],
    { stdio: 'inherit' }
  );
}
run('rustup', ['toolchain', 'install', channel, '--profile', 'minimal'], repoRoot);

// Device only. A simulator slice would double the cargo time for a slice that an
// internal-distribution build can never run; add `aarch64-apple-ios-sim` back
// only if a profile sets `ios.simulator`.
const targets =
  platform === 'ios' ? ['aarch64-apple-ios'] : ['aarch64-linux-android', 'x86_64-linux-android'];
run('rustup', ['target', 'add', '--toolchain', channel, ...targets], repoRoot);

// `--legacy-peer-deps` because the module's lint scaffolding is self-conflicting
// (@eslint/js@10 declares a peer on eslint ^10 while the lockfile pins 9), which
// npm treats as fatal even though nothing here builds with eslint. The two
// binaries this needs — ubrn and bob — resolve fine either way.
if (!existsSync(join(coreModule, 'node_modules', '.bin', 'ubrn'))) {
  run('npm', ['ci', '--include=dev', '--ignore-scripts', '--legacy-peer-deps'], coreModule);
}

const buildArgs =
  platform === 'ios'
    ? ['ubrn', 'build', 'ios', '--release', '--no-sim', '--and-generate']
    : ['ubrn', 'build', 'android', '--release', '--and-generate'];
run('npx', buildArgs, coreModule);
run('npx', ['bob', 'build'], coreModule);

console.log('\n✓ native core ready — the app install can now resolve oximux-core');
