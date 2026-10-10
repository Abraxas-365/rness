import { defineCollection } from 'astro:content'
import { glob } from 'astro/loaders'
import { docsSchema } from '@astrojs/starlight/schema'
import { docId, metadata } from '../scripts/docs.mjs'

const source = glob({
  base: '../docs',
  pattern: '**/*.md',
  generateId: ({ entry }) => docId(entry),
})

export const collections = {
  docs: defineCollection({
    loader: {
      name: 'rness-docs',
      load: context => source.load({
        ...context,
        parseData: entry => context.parseData({
          ...entry,
          data: { ...metadata(entry.filePath!), ...entry.data },
        }),
      }),
    },
    schema: docsSchema(),
  }),
}
