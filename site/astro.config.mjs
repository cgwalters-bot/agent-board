// The documentation site, as agentic-job builds its own
// (cgwalters-forge/agentic-job site/astro.config.mjs): the same layout and
// versions, so the two read as one family and pass the same checks.
import { readdirSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import { defineConfig } from "astro/config";
import { unified } from "@astrojs/markdown-remark";
import starlight from "@astrojs/starlight";
import { DOCS, pageId, remarkRepositoryLinks } from "./links.mjs";

const root = fileURLToPath(new URL("..", import.meta.url));
// Where the site is served (`just site-build-pages` sets these for GitHub
// Pages) and the prefix of links to repository
// files that are not pages: the forge's view of the built commit when
// deployed, the checkout itself otherwise, so a local link check also
// proves those files exist.
const base = (process.env.SITE_BASE ?? "/").replace(/\/?$/, "/");
const source = process.env.SITE_SOURCE_URL ?? pathToFileURL(root).href;

export default defineConfig({
  site: process.env.SITE_URL,
  base,
  trailingSlash: "always",
  markdown: {
    // No typographic replacements, as on the forge: `--flag` in prose stays.
    processor: unified({
      smartypants: false,
      remarkPlugins: [[remarkRepositoryLinks, { root, base, source }]],
    }),
  },
  integrations: [
    starlight({
      title: "agent-board",
      components: { Banner: "./src/components/Banner.astro" },
      sidebar: [
        { label: "Overview", slug: pageId("README.md") },
        // Starlight's autogenerate reads src/content/docs/, not the
        // repository's docs/, so the group lists that directory itself.
        {
          label: "Documentation",
          items: readdirSync(new URL(`../${DOCS}`, import.meta.url))
            .filter((name) => name.endsWith(".md"))
            .sort()
            .map((name) => pageId(`${DOCS}${name}`)),
        },
      ],
    }),
  ],
});
