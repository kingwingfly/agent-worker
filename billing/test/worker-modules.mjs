import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('../', import.meta.url))

// Use the same module types as Wrangler's billing configuration.
export const billingModules = {
  modulesRoot: root,
  modules: [
    ...['worker', 'ledger', 'chain', 'policy', 'migration'].map(name => ({ type: 'ESModule', path: root + 'src/' + name + '.js' })),
    ...['index.html', 'admin.html', 'assets/style.css', 'assets/admin.js', 'assets/common.js', 'assets/checkout.js', 'assets/nunito-OFL.txt']
      .map(name => ({ type: 'Text', path: root + 'public/' + name })),
    ...['wallet-cat.webp', 'nunito-latin.woff2']
      .map(name => ({ type: 'Data', path: root + 'public/assets/' + name }))
  ]
}
