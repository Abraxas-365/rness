import { test } from 'node:test'
import assert from 'node:assert/strict'
import path from 'node:path'
import { docId, docsRoot, metadata, resolveLink, repository, base, allSlugs, sidebarSlugs } from './docs.mjs'

const tutorial = path.join(docsRoot, 'tutorials/first-configuration.md')
test('routes preserve directories and collapse README/index', () => {
  assert.equal(docId('README.md'), 'docs')
  assert.equal(docId('contributing/README.md'), 'docs/contributing')
  assert.equal(docId('reference/lua/task.md'), 'docs/reference/lua/task')
})
test('metadata comes from source Markdown and edit link points to source', () => {
  const data = metadata(tutorial)
  assert.equal(data.title, 'First configuration and session')
  assert.match(data.description, /^Outcome: run a session/)
  assert.equal(data.editUrl, `${repository}/edit/main/docs/tutorials/first-configuration.md`)
})
test('rewrites document links while preserving anchors and external URLs', () => {
  assert.equal(base, process.env.SITE_BASE ? `/${process.env.SITE_BASE.replace(/^\/+|\/+$/g, '')}/` : '/')
  const b = base.slice(0, -1)
  assert.equal(resolveLink('../guides/installation/from-source.md', tutorial), `${b}/docs/guides/installation/from-source/`)
  assert.equal(resolveLink('../reference/configuration/providers.md#multiple-accounts-per-provider', tutorial), `${b}/docs/reference/configuration/providers/#multiple-accounts-per-provider`)
  assert.equal(resolveLink('#before-you-begin', tutorial), '#before-you-begin')
  assert.equal(resolveLink('https://example.com/a.md', tutorial), 'https://example.com/a.md')
  assert.equal(resolveLink('../README.md', tutorial), `${b}/docs/`)
  assert.equal(resolveLink('../../README.md#get-started', tutorial), `${repository}/blob/main/README.md#get-started`)
  assert.equal(resolveLink('../../examples/workflows/', tutorial), `${repository}/tree/main/examples/workflows`)
})
test('curated sidebar lists every document exactly once', async () => {
  const { sidebar } = await import('../src/sidebar.mjs')
  const listed = sidebarSlugs(sidebar)
  assert.deepEqual([...new Set(listed)].sort(), allSlugs().sort())
  assert.equal(listed.length, new Set(listed).size, 'a document appears twice in the sidebar')
})
test('fails on missing source files rather than publishing broken links', () => {
  assert.throws(() => resolveLink('../missing.md', tutorial), /Broken source link/)
})
