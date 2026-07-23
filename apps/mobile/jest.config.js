/**
 * Jest setup for the phone app.
 *
 * `transformIgnorePatterns` is the load-bearing part. Jest does not transform
 * anything under `node_modules` by default, but a large share of the React
 * Native ecosystem — including `react-native-marked` and the `marked` parser
 * beneath it — ships untranspiled ESM. Without an explicit allowlist those
 * packages reach the runtime as raw `import` statements and every test that
 * touches them dies on a syntax error that points at the dependency rather than
 * at anything we wrote.
 */
module.exports = {
  preset: 'jest-expo',
  setupFiles: ['<rootDir>/jest.setup.js'],
  transformIgnorePatterns: [
    'node_modules/(?!(' +
      [
        '(jest-)?react-native',
        '@react-native(-community)?',
        // `[\\w-]*` matters: the bare `expo(nent)?` form used by most published
        // configs requires a `/` straight after "expo", so it silently misses
        // every hyphenated package — `expo-modules-core` above all, which the
        // jest-expo preset itself loads during setup.
        'expo(nent)?[\\w-]*',
        '@expo(nent)?/.*',
        '@expo-google-fonts/.*',
        'react-navigation',
        '@react-navigation/.*',
        '@sentry/react-native',
        'native-base',
        'react-native-svg',
        'react-native-marked',
        'react-native-code-highlighter',
        'react-syntax-highlighter',
        'react-native-reanimated-table',
        '@jsamr/.*',
        // `"type": "module"` — enumerated by walking the dependency tree of the
        // three markdown/highlight packages and reading each package.json,
        // rather than discovering them one failing test at a time. None are
        // named by our own imports; they are all transitive.
        'marked',
        'github-slugger',
        'html-entities',
        'trim-newlines',
        'refractor',
        'hastscript',
        'parse-entities',
      ].join('|') +
      ')/)',
  ],
  moduleNameMapper: {
    '^@/(.*)$': '<rootDir>/src/$1',
    // Its package.json `react-native`/`module` export condition points at an
    // untranspiled `.mjs` build, which the `\.[jt]sx?$` transform below never
    // matches — every icon import (not just the `type` ones) would otherwise
    // reach the runtime as a raw `export` statement. The `main` condition is
    // plain CommonJS, so redirecting here sidesteps the transform gap entirely
    // rather than teaching Jest a new extension to parse.
    '^lucide-react-native$': '<rootDir>/node_modules/lucide-react-native/dist/cjs/lucide-react-native.js',
  },
  testMatch: ['**/*.test.ts', '**/*.test.tsx'],
};
