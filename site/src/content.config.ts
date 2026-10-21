import { readFile } from "node:fs/promises";
import { defineCollection } from "astro:content";
import { glob } from "astro/loaders";
import { z } from "astro/zod";
import { docsSchema } from "@astrojs/starlight/schema";
import { DOCS, PAGES, firstHeading, llmNotice, pageId } from "../links.mjs";

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
            const markdown = entry.filePath ? await readFile(entry.filePath, "utf8") : "";
            if (entry.data.title === undefined && entry.filePath) {
              const title = firstHeading(markdown);
              if (title === null) throw new Error(`${entry.filePath}: no front matter title and no leading # heading`);
              entry = { ...entry, data: { ...entry.data, title } };
            }
            // The banner comes from the source's notice alone, never from
            // front matter, so a page shows it exactly when its text has it.
            const { llmNotice: _, ...data } = entry.data;
            const notice = llmNotice(markdown);
            entry = { ...entry, data: notice === null ? data : { ...data, llmNotice: notice } };
            return context.parseData(entry);
          },
        }),
    },
    // llmNotice: the context sentences of the page's LLM notice, set when
    // the page has one (see llmNotice in ../links.mjs).
    schema: docsSchema({ extend: z.object({ llmNotice: z.string().optional() }) }),
  }),
};
