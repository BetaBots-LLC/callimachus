/// <reference types="vite/client" />
import type { ReactNode } from "react";
import { HeadContent, Outlet, Scripts, createRootRoute } from "@tanstack/react-router";
import appCss from "@/styles/app.css?url";
import { DESCRIPTION, TAGLINE } from "@/lib/site";
import { seo } from "@/lib/seo";
import { Header } from "@/components/site/Header";
import { Footer } from "@/components/site/Footer";

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: "utf-8" },
      { name: "viewport", content: "width=device-width, initial-scale=1" },
      { name: "theme-color", content: "#1c160f" },
      ...seo({ title: `Callimachus — ${TAGLINE}`, description: DESCRIPTION }),
    ],
    links: [
      { rel: "stylesheet", href: appCss },
      { rel: "icon", href: "/favicon.svg", type: "image/svg+xml" },
      { rel: "apple-touch-icon", href: "/icon.png" },
      { rel: "manifest", href: "/site.webmanifest" },
    ],
  }),
  // shellComponent renders the HTML document; component renders the app layout
  // (with the route Outlet). Putting the <html> shell in `component` instead is
  // what caused the client hydration "useContext null" crash.
  shellComponent: RootDocument,
  component: RootLayout,
});

function RootDocument({ children }: Readonly<{ children: ReactNode }>) {
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        {children}
        <Scripts />
      </body>
    </html>
  );
}

function RootLayout() {
  return (
    <div className="flex min-h-dvh flex-col">
      <Header />
      <div className="flex-1">
        <Outlet />
      </div>
      <Footer />
    </div>
  );
}
