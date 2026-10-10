import { defineConfig } from 'astro/config'
import starlight from '@astrojs/starlight'
import { unified } from '@astrojs/markdown-remark'
import { remarkDocs, repository, site, base } from './scripts/docs.mjs'
import { sidebar } from './src/sidebar.mjs'

export default defineConfig({
  // GitHub Pages serves the project at https://abraxas-365.github.io/rness/ (SITE_BASE=/rness in CI).
  site,
  base,
  trailingSlash: 'always',
  markdown: { processor: unified({ remarkPlugins: [remarkDocs] }) },
  integrations: [starlight({
    title: 'rness',
    description: 'A terminal-first coding agent with a Rust engine and a Lua-configurable workflow. Tutorials, guides and reference documentation.',
    logo: { src: './src/assets/rness-mark.svg' },
    social: [{ icon: 'github', label: 'GitHub', href: repository }],
    customCss: ['./src/styles/theme.css'],
    expressiveCode: { themes: ['gruvbox-dark-medium', 'gruvbox-light-medium'], styleOverrides: { borderRadius: '0.25rem' } },
    sidebar,
  })],
})
