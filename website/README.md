# rness website

Static landing page and documentation built with Astro + Starlight. Requires Node
22.12+ and npm. Run from a checkout of the whole repository:

```sh
cd website
npm ci
npm run dev
```

Open <http://127.0.0.1:4330/> for the landing page and `/docs/` for documentation.
For the production build, including Pagefind search, use
`npm run build && npm run preview`. Preview serves the last build; rebuild after
changes. Development mode does not provide the production search index.

## Content and styling

- `../docs/**/*.md` is the only documentation source. No generated Markdown,
  copies or front matter migration. The loader derives titles and descriptions
  from each page's H1 and first paragraph; the remark adapter removes the
  duplicate H1 and rewrites local Markdown links.
- Documentation lives under `/docs/`. `src/pages/index.astro` is the custom
  landing page at `/`, styled with `src/styles/landing.css`. Its theme toggle
  persists the same preference as Starlight.
- Links to repository files outside `docs/` (crates, examples, flavors) go to
  GitHub. A link to a missing file fails the build.
- `src/styles/tokens.css` owns the Gruvbox colors (matching
  `flavors/default/lua/theme.lua`), fonts and radius. `src/styles/theme.css`
  maps them to Starlight's variables; code blocks use the Gruvbox
  Expressive Code themes.
- The sidebar is curated in `src/sidebar.mjs` (reading order, nested groups).
  Titles come from each page's H1. `npm test` fails when a document under
  `../docs` is missing from the sidebar or listed twice, so add new pages there.
- The terminal on the landing page is an HTML illustration, labeled as such,
  not a screenshot. Landing copy follows the root README; keep install commands
  and claims in sync with it.

## Validation

```sh
npm test
npm run check
npm run build
npm run check:links
```

`check:links` checks rendered local links, anchors and assets, not remote URLs.

No production domain or deployment is configured. The build skips sitemap
creation until `site` is set in `astro.config.mjs`; Starlight also logs warnings
for its empty optional i18n collection and default 404 fallback.
