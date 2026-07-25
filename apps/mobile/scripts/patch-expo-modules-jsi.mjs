// expo-modules-jsi 57.0.3 does not compile under Xcode 26.3: Swift's type
// checker rejects `abs(milliseconds) <= maxJavaScriptDateMilliseconds` in
// JavaScriptCodable+Date.swift as "ambiguous without a type annotation", which
// fails the whole iOS build before any of our own code is reached.
//
// `.magnitude` is the same value for a Double but resolves unambiguously.
//
// This is a postinstall script rather than a patch-package patch because
// expo-modules-jsi ships symlinked source directories, which patch-package
// refuses to diff. Re-check on every Expo bump: once upstream fixes this, the
// script no-ops (the search string stops matching) and can be deleted.
import { readFileSync, writeFileSync } from 'node:fs';

const FILE =
  'node_modules/expo-modules-jsi/apple/Sources/ExpoModulesJSI/Coding/JavaScriptCodable+Date.swift';

const BROKEN = 'abs(milliseconds) <= maxJavaScriptDateMilliseconds';
const FIXED = 'milliseconds.magnitude <= maxJavaScriptDateMilliseconds';

let source;
try {
  source = readFileSync(FILE, 'utf8');
} catch {
  // Package not installed (or upstream moved the file) — nothing to do.
  process.exit(0);
}

if (source.includes(FIXED)) {
  process.exit(0); // already applied, or upstream fixed it the same way
}

if (!source.includes(BROKEN)) {
  console.warn(
    `[patch-expo-modules-jsi] neither the broken nor the patched expression was found in ${FILE}.\n` +
      'Upstream likely rewrote this code. Verify the iOS build still compiles, then delete this script.'
  );
  process.exit(0);
}

writeFileSync(FILE, source.replace(BROKEN, FIXED));
console.log('[patch-expo-modules-jsi] applied the Xcode 26.3 Swift type-inference fix.');
