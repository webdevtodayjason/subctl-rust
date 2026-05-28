// esbuild entry. Bundles src/entry.tsx → dist/bundle.js (single-file ESM).
//
// Externalizes nothing (Ink + React go into the bundle). Adds a #!/usr/bin/env
// node shebang and chmod +x so the bundle is directly runnable, even though
// the production launcher is bin/subctl which calls `node <bundle>` explicitly.
import { build } from 'esbuild'
import { chmodSync, writeFileSync, readFileSync, mkdirSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'

const __dirname = dirname(fileURLToPath(import.meta.url))
const root = resolve(__dirname, '..')
const outFile = resolve(root, 'dist/bundle.js')

mkdirSync(resolve(root, 'dist'), { recursive: true })

await build({
  entryPoints: [resolve(root, 'src/entry.tsx')],
  outfile: outFile,
  bundle: true,
  platform: 'node',
  format: 'esm',
  target: 'node22',
  jsx: 'automatic',
  sourcemap: 'inline',
  // Optional dev-only dep of Ink — only loaded when DEV=true. We replace
  // it with an empty stub at build time so the bundle resolves cleanly
  // and shrinks. The devtools surface stays unavailable in shipped builds.
  plugins: [
    {
      name: 'stub-devtools',
      setup(build) {
        build.onResolve({ filter: /^react-devtools-core$/ }, (args) => ({
          path: args.path,
          namespace: 'stub-devtools'
        }))
        build.onLoad({ filter: /.*/, namespace: 'stub-devtools' }, () => ({
          contents:
            'const stub = { connectToDevTools: () => {} }; export default stub; export const connectToDevTools = () => {}',
          loader: 'js'
        }))
      }
    }
  ],
  banner: {
    js: [
      '#!/usr/bin/env node',
      "import { createRequire } from 'node:module'",
      "const require = createRequire(import.meta.url)"
    ].join('\n')
  },
  logLevel: 'info'
})

chmodSync(outFile, 0o755)

console.log('[build] wrote', outFile)
