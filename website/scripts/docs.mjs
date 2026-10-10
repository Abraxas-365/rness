import { readdirSync, readFileSync, existsSync, statSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { visit } from 'unist-util-visit'

export const root = fileURLToPath(new URL('../../', import.meta.url))
export const docsRoot = path.join(root, 'docs')
export const repository = 'https://github.com/Abraxas-365/rness'

export function docId(file) {
  return `docs/${file.replace(/\\/g, '/').replace(/\.md$/, '').replace(/(^|\/)(index|README)$/, '')}`.replace(/\/$/, '')
}

// One curated sidebar entry; the label defaults to the page's own title.
export function page(source, label) {
  const file = path.join(docsRoot, `${source}.md`)
  return { label: label ?? metadata(file).title, slug: docId(`${source}.md`) }
}

export function group(label, items, collapsed = true) {
  return { label, collapsed, items }
}

// Every published Markdown document, as slugs, to prove the curated sidebar leaves none out.
export function allSlugs() {
  return readdirSync(docsRoot, { recursive: true })
    .filter(file => file.endsWith('.md'))
    .map(file => docId(file))
}

export function sidebarSlugs(items) {
  return items.flatMap(item => item.items ? sidebarSlugs(item.items) : [item.slug])
}

export function metadata(file) {
  const source = readFileSync(file, 'utf8')
  const title = source.match(/^# (.+)$/m)?.[1]
  if (!title) throw new Error(`Missing document title: ${file}`)
  const paragraph = source.slice(source.indexOf('\n') + 1).trim().split(/\n\s*\n/)[0]
  const description = paragraph.replace(/\[([^\]]+)\]\([^)]+\)/g, '$1').replace(/[`*_]/g, '').replace(/\s+/g, ' ').slice(0, 160)
  return { title, description, editUrl: `${repository}/edit/main/${path.relative(root, file).split(path.sep).join('/')}` }
}

// Preserve GitHub-friendly source links; only their rendered website URLs change.
export function resolveLink(url, file) {
  if (/^(?:[a-z][a-z\d+.-]*:|\/|#)/i.test(url)) return url
  const split = url.search(/[?#]/)
  const pathname = split === -1 ? url : url.slice(0, split)
  const suffix = split === -1 ? '' : url.slice(split)
  const target = path.resolve(path.dirname(file), decodeURIComponent(pathname))
  const relative = path.relative(root, target).split(path.sep).join('/')
  if (relative.startsWith('../') || !existsSync(target)) throw new Error(`Broken source link in ${file}: ${url}`)
  if (target.startsWith(docsRoot + path.sep) && target.endsWith('.md')) {
    return `/${docId(path.relative(docsRoot, target))}/${suffix}`
  }
  const kind = statSync(target).isDirectory() ? 'tree' : 'blob'
  return `${repository}/${kind}/main/${relative}${suffix}`
}

export function remarkDocs() {
  return (tree, file) => {
    if (!file.path?.startsWith(docsRoot + path.sep)) return
    // Starlight renders the title as #_top; avoid a duplicate H1.
    const heading = tree.children.find(node => node.type === 'heading' && node.depth === 1)
    if (heading) tree.children.splice(tree.children.indexOf(heading), 1)
    visit(tree, node => {
      if (node.type === 'link' || node.type === 'definition') node.url = resolveLink(node.url, file.path)
      // Images are left relative so Astro fingerprints the existing docs/assets SVGs.
    })
  }
}
