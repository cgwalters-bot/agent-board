import { readFile } from "node:fs/promises";
import { defineCollection } from "astro:content";
import { glob } from "astro/loaders";
import { docsSchema } from "@astrojs/starlight/schema";
import { DOCS, PAGES, firstHeading, pageId } from "../links.mjs";

// The pages are the repository's own Markdown, not copies: the glob
// starts at the repository root. They have no front matter, so a page's
// title is its first heading. Shared with agentic-job
// (cgwalters-forge/agentic-job site/src/content.config.ts).
const pages = glob({
  base: "..",
  pattern: [...Object.keys(PAGES), `${DOCS}*.md`],
  generateId: ({ entry }) => pageId(entry) || "index",
});

export const collections = {
  docs: defineCollection({
    loader: {
      name: "repository-docs",
      load: (context) =>
        pages.load({
          ...context,
          parseData: async (entry) => {
            if (entry.data.title === undefined && entry.filePath) {
              const title = firstHeading(await readFile(entry.filePath, "utf8"));
              if (title === null) throw new Error(`${entry.filePath}: no front matter title and no leading # heading`);
              entry = { ...entry, data: { ...entry.data, title } };
            }
            return context.parseData(entry);
          },
        }),
    },
    schema: docsSchema(),
  }),
};
