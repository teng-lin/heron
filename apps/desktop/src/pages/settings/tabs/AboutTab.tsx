import * as Dialog from "@radix-ui/react-dialog";
import ReactMarkdown from "react-markdown";

// Vite's `?raw` import (declared via `vite/client` reference in
// `vite-env.d.ts`) inlines the file as a string at build time. We bundle
// the notices once rather than reading from disk because the Tauri
// app's `frontendDist` may live anywhere on disk relative to the user's
// vault — relying on the renderer's filesystem access here would
// require a fs:read permission we don't otherwise need.
import licenseNotices from "../../../../THIRD_PARTY_NOTICES.md?raw";

import { Button } from "../../../components/ui/button";
import {
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "../../../components/ui/dialog";

export function AboutTab() {
  return (
    <section className="space-y-4">
      <h2 className="text-lg font-medium">About</h2>
      <dl className="grid grid-cols-[8rem_1fr] gap-y-2 text-sm">
        <dt className="text-muted-foreground">Version</dt>
        <dd>{__APP_VERSION__}</dd>
        <dt className="text-muted-foreground">Build</dt>
        <dd>{__APP_BUILD__}</dd>
      </dl>
      <Dialog.Root>
        <DialogTrigger asChild>
          <Button variant="outline">View licenses…</Button>
        </DialogTrigger>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Third-party notices</DialogTitle>
          </DialogHeader>
          {/*
           * `prose` styling lives inline since the design system doesn't
           * (yet) ship a Tailwind typography preset. The component
           * overrides give us readable headings + spacing without
           * a `prose` plugin dep. Limiting to common nodes — paragraph,
           * heading, link, list, code, strong — matches the markdown
           * subset the bundled notices file uses.
           */}
          <div className="text-sm leading-relaxed space-y-3">
            <ReactMarkdown
              components={{
                h1: ({ children }) => (
                  <h1 className="text-lg font-semibold mt-4 first:mt-0">
                    {children}
                  </h1>
                ),
                h2: ({ children }) => (
                  <h2 className="text-base font-semibold mt-4">{children}</h2>
                ),
                h3: ({ children }) => (
                  <h3 className="text-sm font-semibold mt-3">{children}</h3>
                ),
                p: ({ children }) => <p className="my-2">{children}</p>,
                a: ({ href, children }) => (
                  <a
                    href={href}
                    className="underline text-primary hover:opacity-80"
                    target="_blank"
                    rel="noopener noreferrer"
                  >
                    {children}
                  </a>
                ),
                ul: ({ children }) => (
                  <ul className="list-disc pl-6 my-2 space-y-1">{children}</ul>
                ),
                ol: ({ children }) => (
                  <ol className="list-decimal pl-6 my-2 space-y-1">
                    {children}
                  </ol>
                ),
                code: ({ children }) => (
                  <code className="font-mono text-xs bg-muted px-1 py-0.5 rounded">
                    {children}
                  </code>
                ),
                strong: ({ children }) => (
                  <strong className="font-semibold">{children}</strong>
                ),
                hr: () => <hr className="my-4 border-border" />,
              }}
            >
              {licenseNotices}
            </ReactMarkdown>
          </div>
        </DialogContent>
      </Dialog.Root>
      <p className="text-xs text-muted-foreground">
        heron is private, on-device, and AGPL-3.0-or-later licensed.
      </p>
    </section>
  );
}
